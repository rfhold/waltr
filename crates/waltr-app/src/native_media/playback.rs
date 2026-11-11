//! CPAL streams are created, used, and dropped on one retained owner thread.
use super::{CHUNK, Error, Fence};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub(super) trait EffectFence: Clone + Send + Sync + 'static {
    fn valid(&self) -> bool;
    fn cancel(&self) -> &tokio_util::sync::CancellationToken;
}
impl EffectFence for Fence {
    fn valid(&self) -> bool { self.valid() }
    fn cancel(&self) -> &tokio_util::sync::CancellationToken { &self.cancel }
}

pub(super) struct Packet {
    bytes: Vec<u8>,
    done: oneshot::Sender<Result<(), Error>>,
    _credit: Option<tokio::sync::OwnedSemaphorePermit>,
}
#[derive(Clone)]
pub(super) struct Sender {
    channel: mpsc::Sender<Option<Packet>>,
    credits: Arc<tokio::sync::Semaphore>,
}
pub(super) struct Owner {
    sender: Sender,
    stop: tokio_util::sync::CancellationToken,
    thread: Option<std::thread::JoinHandle<Result<(), Error>>>,
    complete: oneshot::Receiver<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adaptation {
    Exact,
    Mono24ToStereo48,
}

fn select_config(
    source: waltr_client::interaction::wire::Pcm16Le,
    configs: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
) -> Result<(cpal::SupportedStreamConfig, Adaptation), Error> {
    let mut fallback = None;
    for config in configs {
        if !matches!(config.sample_format(), cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16) {
            continue;
        }
        let supports = |rate| config.min_sample_rate().0 <= rate && config.max_sample_rate().0 >= rate;
        if u32::from(config.channels()) == source.channels && supports(source.sample_rate) {
            return Ok((config.with_sample_rate(cpal::SampleRate(source.sample_rate)), Adaptation::Exact));
        }
        if source.sample_rate == 24000 && source.channels == 1 && config.channels() == 2 && supports(48000) && fallback.is_none() {
            fallback = Some((config.with_sample_rate(cpal::SampleRate(48000)), Adaptation::Mono24ToStereo48));
        }
    }
    fallback.ok_or(Error::Unsupported)
}

struct Converter {
    adaptation: Adaptation,
    previous: Option<i16>,
}
impl Converter {
    fn new(adaptation: Adaptation) -> Self { Self { adaptation, previous: None } }

    fn packet(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        if bytes.len() > CHUNK || !bytes.len().is_multiple_of(2) { return Err(Error::Unsupported); }
        if self.adaptation == Adaptation::Exact { return Ok(bytes); }
        let mut output = Vec::with_capacity(bytes.len() * 4);
        for sample in bytes.chunks_exact(2) {
            let current = i16::from_le_bytes([sample[0], sample[1]]);
            let previous = self.previous.unwrap_or(current);
            // Causal linear interpolation: midpoint then current, with a held left endpoint.
            // One source sample of history crosses packets; no deferred EOF samples exist.
            let midpoint = ((i32::from(previous) + i32::from(current)) / 2) as i16;
            for value in [midpoint, current] {
                output.extend_from_slice(&value.to_le_bytes());
                output.extend_from_slice(&value.to_le_bytes());
            }
            self.previous = Some(current);
        }
        Ok(output)
    }
}

fn buffer_duration(samples: usize, format: waltr_client::interaction::wire::Pcm16Le) -> std::time::Duration {
    std::time::Duration::from_secs_f64(samples as f64 / f64::from(format.sample_rate) / f64::from(format.channels))
}
struct Queue {
    packets: std::collections::VecDeque<Packet>,
    offset: usize,
    failed: bool,
    active: bool,
    ended: bool,
    scheduled_until: Option<std::time::Instant>,
    last_callback: std::time::Instant,
}
impl Queue {
    fn drained(&self, now: std::time::Instant) -> bool {
        self.ended
            && self.packets.is_empty()
            && self.scheduled_until.is_some_and(|until| now >= until)
    }
    fn scheduled(
        &mut self,
        now: std::time::Instant,
        delay: std::time::Duration,
        duration: std::time::Duration,
        had_audio: bool,
    ) {
        if delay + duration > std::time::Duration::from_secs(2) {
            self.failed = true;
            return;
        }
        if had_audio || (self.active && self.scheduled_until.is_none()) {
            self.scheduled_until = Some(now + delay + duration);
        }
    }
}
pub(super) fn available() -> bool {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.supported_output_configs().ok())
        .is_some_and(|mut configs| {
            configs.any(|c| {
                matches!(c.channels(), 1 | 2)
                    && matches!(
                        c.sample_format(),
                        cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
                    )
            })
        })
}
fn fill<T: Copy>(
    output: &mut [T],
    queue: &Arc<Mutex<Queue>>,
    fence: &impl EffectFence,
    convert: impl Fn(i16) -> T,
) {
    let mut queue = queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    queue.last_callback = std::time::Instant::now();
    for sample in output {
        // This is the physical output boundary, not merely the enqueue boundary.
        if !fence.valid() || queue.failed {
            *sample = convert(0);
            continue;
        }
        if !queue.active && (queue.packets.len() >= 2 || queue.ended) {
            queue.active = true;
        }
        if !queue.active {
            *sample = convert(0);
            continue;
        }
        if queue.packets.is_empty() && !queue.ended {
            queue.failed = true;
            *sample = convert(0);
            continue;
        }
        let value = queue
            .packets
            .front()
            .map(|p| i16::from_le_bytes([p.bytes[queue.offset], p.bytes[queue.offset + 1]]))
            .unwrap_or(0);
        *sample = convert(value);
        if !queue.packets.is_empty() {
            queue.offset += 2;
            if queue
                .packets
                .front()
                .is_some_and(|p| queue.offset == p.bytes.len())
            {
                if let Some(packet) = queue.packets.pop_front() {
                    let _sent = packet.done.send(Ok(()));
                }
                queue.offset = 0;
            }
        }
    }
}
fn callback<T: cpal::Sample + cpal::FromSample<i16>>(
    data: &mut [T],
    info: &cpal::OutputCallbackInfo,
    queue: &Arc<Mutex<Queue>>,
    fence: &impl EffectFence,
    format: waltr_client::interaction::wire::Pcm16Le,
) {
    let timestamp = info.timestamp();
    let Some(delay) = timestamp.playback.duration_since(&timestamp.callback) else {
        queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failed = true;
        fill(data, queue, fence, T::from_sample);
        return;
    };
    if delay > std::time::Duration::from_secs(2) {
        queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failed = true;
    }
    let duration = buffer_duration(data.len(), format);
    let had_audio = !queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .packets
        .is_empty();
    fill(data, queue, fence, T::from_sample);
    let mut q = queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    q.scheduled(std::time::Instant::now(), delay, duration, had_audio);
}
impl Owner {
    pub fn start(
        format: waltr_client::interaction::wire::Pcm16Le,
        fence: impl EffectFence,
    ) -> Result<Self, Error> {
        if !matches!(format.channels, 1 | 2)
            || !(8000..=192000).contains(&format.sample_rate)
            || !fence.valid()
        {
            return Err(Error::Unsupported);
        }
        let (channel, mut receiver) = mpsc::channel::<Option<Packet>>(5);
        let sender = Sender {
            channel,
            credits: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        let stop = tokio_util::sync::CancellationToken::new();
        let stopping = stop.clone();
        let (done, complete) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("waltr-playback".into())
            .spawn(move || {
                let result = (|| {
                    let device = cpal::default_host()
                        .default_output_device()
                        .ok_or(Error::Unsupported)?;
                    let (config, adaptation) = select_config(format, device.supported_output_configs().map_err(|_| Error::Unsupported)?)?;
                    let mut converter = Converter::new(adaptation);
                    let device_format = waltr_client::interaction::wire::Pcm16Le {
                        sample_rate: config.sample_rate().0,
                        channels: u32::from(config.channels()),
                    };
                    let queue = Arc::new(Mutex::new(Queue {
                        packets: std::collections::VecDeque::new(),
                        offset: 0,
                        failed: false,
                        active: false,
                        ended: false,
                        scheduled_until: None,
                        last_callback: std::time::Instant::now(),
                    }));
                    let q = queue.clone();
                    let f = fence.clone();
                    let e = queue.clone();
                    let error = move |_error| {
                        e.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .failed = true;
                    };
                    let stream = match config.sample_format() {
                        cpal::SampleFormat::F32 => device.build_output_stream(
                            &config.config(),
                            move |data: &mut [f32], info| callback(data, info, &q, &f, device_format),
                            error,
                            None,
                        ),
                        cpal::SampleFormat::I16 => device.build_output_stream(
                            &config.config(),
                            move |data: &mut [i16], info| callback(data, info, &q, &f, device_format),
                            error,
                            None,
                        ),
                        cpal::SampleFormat::U16 => device.build_output_stream(
                            &config.config(),
                            move |data: &mut [u16], info| callback(data, info, &q, &f, device_format),
                            error,
                            None,
                        ),
                        _ => return Err(Error::Unsupported),
                    }
                    .map_err(|_| Error::Unsupported)?;
                    if !fence.valid() {
                        return Err(Error::Cancelled);
                    }
                    stream.play().map_err(|_| Error::Interaction)?;
                    let mut result = Ok(());
                    loop {
                        if stopping.is_cancelled() || !fence.valid() {
                            break;
                        }
                        let mut q = queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if q.failed {
                            result = Err(Error::Interaction);
                            break;
                        }
                        if q.last_callback.elapsed() > std::time::Duration::from_secs(2) {
                            result = Err(Error::Interaction);
                            break;
                        }
                        while q.packets.len() < 4 && !q.ended {
                            match receiver.try_recv() {
                                Ok(Some(mut packet)) => {
                                    packet.bytes = converter.packet(packet.bytes)?;
                                    q.packets.push_back(packet);
                                }
                                Ok(None) => q.ended = true,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    q.failed = true;
                                    break;
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                            }
                        }
                        if q.drained(std::time::Instant::now()) {
                            break;
                        }
                        drop(q);
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    drop(stream);
                    if let Some(packet) = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .packets
                        .pop_front()
                    {
                        let _sent = packet.done.send(Err(Error::Cancelled));
                    }
                    result
                })();
                let _sent = done.send(());
                result
            })
            .map_err(|_| Error::Interaction)?;
        Ok(Self {
            sender,
            stop,
            thread: Some(thread),
            complete,
        })
    }
    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }
    pub async fn finish(&mut self, success: bool) -> Result<(), Error> {
        if self.thread.is_none() {
            return Ok(());
        }
        if !success || self.sender.channel.send(None).await.is_err() {
            self.stop.cancel();
        }
        // Keep the actual thread handle in the owner across cancellation of this wait.
        let _completed = (&mut self.complete).await;
        self.thread
            .take()
            .ok_or(Error::Interaction)?
            .join()
            .map_err(|_| Error::Interaction)?
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}
pub(super) async fn send(sender: &Sender, bytes: Vec<u8>, fence: &impl EffectFence) -> Result<(), Error> {
    if bytes.is_empty() || bytes.len() > CHUNK || !bytes.len().is_multiple_of(2) || !fence.valid() {
        return Err(Error::Unsupported);
    }
    let (done, receipt) = oneshot::channel();
    let credit = tokio::select! { biased; () = fence.cancel().cancelled() => return Err(Error::Cancelled), result = sender.credits.clone().acquire_owned() => result.map_err(|_| Error::Interaction)? };
    drop(receipt);
    tokio::select! { biased; () = fence.cancel().cancelled() => Err(Error::Cancelled), result = sender.channel.send(Some(Packet { bytes, done, _credit: Some(credit) })) => result.map_err(|_| Error::Interaction) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pcm(rate: u32, channels: u32) -> waltr_client::interaction::wire::Pcm16Le {
        waltr_client::interaction::wire::Pcm16Le { sample_rate: rate, channels }
    }
    fn config(rate: u32, channels: u16) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(channels, cpal::SampleRate(rate), cpal::SampleRate(rate), cpal::SupportedBufferSize::Unknown, cpal::SampleFormat::F32)
    }
    fn bytes(samples: &[i16]) -> Vec<u8> { samples.iter().flat_map(|s| s.to_le_bytes()).collect() }
    fn samples(bytes: &[u8]) -> Vec<i16> { bytes.chunks_exact(2).map(|s| i16::from_le_bytes([s[0],s[1]])).collect() }

    #[test]
    fn conversion_selection_prefers_exact_and_only_adapts_24_mono_to_48_stereo() {
        let (chosen, mode) = select_config(pcm(24000, 1), [config(48000, 2), config(24000, 1)]).unwrap();
        assert_eq!(mode, Adaptation::Exact);
        assert_eq!(chosen.sample_rate().0, 24000);
        let (chosen, mode) = select_config(pcm(24000, 1), [config(48000, 2)]).unwrap();
        assert_eq!(mode, Adaptation::Mono24ToStereo48);
        assert_eq!(chosen.channels(), 2);
        assert_eq!(chosen.sample_rate().0, 48000);
        for source in [pcm(22050, 1), pcm(24000, 2), pcm(48000, 1)] {
            assert!(select_config(source, [config(48000, 2)]).is_err());
        }
        assert!(select_config(pcm(24000, 1), [config(44100, 2), config(48000, 1)]).is_err());
        assert_eq!(select_config(pcm(48000, 2), [config(48000, 2)]).unwrap().1, Adaptation::Exact);
    }

    #[test]
    fn conversion_is_continuous_bounded_and_has_no_deferred_eof_tail() {
        let input = [i16::MIN, i16::MAX, -5, 7, 100];
        let mut whole = Converter::new(Adaptation::Mono24ToStereo48);
        let expected = whole.packet(bytes(&input)).unwrap();
        assert_eq!(samples(&expected), [-32768,-32768,-32768,-32768, 0,0,32767,32767, 16381,16381,-5,-5, 1,1,7,7, 53,53,100,100]);
        for split in 0..=input.len() {
            let mut converter = Converter::new(Adaptation::Mono24ToStereo48);
            let mut actual = converter.packet(bytes(&input[..split])).unwrap();
            actual.extend(converter.packet(Vec::new()).unwrap());
            actual.extend(converter.packet(bytes(&input[split..])).unwrap());
            assert_eq!(actual, expected, "split={split}");
            assert!(converter.packet(Vec::new()).unwrap().is_empty());
        }
        assert_eq!(expected.len(), input.len() * 8);
        assert_eq!(&samples(&expected)[expected.len()/2-4..], &[53,53,100,100]);
        let mut converter = Converter::new(Adaptation::Mono24ToStereo48);
        assert!(converter.packet(Vec::new()).unwrap().is_empty());
        assert_eq!(converter.packet(vec![0;CHUNK]).unwrap().len(), CHUNK*4);
        assert!(converter.packet(vec![0;CHUNK+2]).is_err());
        assert!(converter.packet(vec![0;3]).is_err());
        let mut exact = Converter::new(Adaptation::Exact);
        assert_eq!(exact.packet(bytes(&input)).unwrap(), bytes(&input));
    }

    #[test]
    fn converted_callback_preserves_stereo_tail_fences_and_device_clock_drain() {
        for revoke in ["none", "cancel", "access", "sink"] {
            let (base, queue) = fixture();
            let fence = crate::native_media::hold_playback::Fence {
                host:base.audio.clone(), generation:base.generation,
                generation_cancel:tokio_util::sync::CancellationToken::new(),
                cancel:tokio_util::sync::CancellationToken::new(),
                revoked:tokio_util::sync::CancellationToken::new(),
                credential:Some((base.credentials.clone(),base.revision)),
            };
            let mut converter = Converter::new(Adaptation::Mono24ToStereo48);
            let mut packet = packet(0);
            packet.bytes = converter.packet(bytes(&[100,300,500])).unwrap();
            { let mut q=queue.lock().unwrap(); q.packets.push_back(packet); q.ended=true; }
            let mut first=[0i16;4];
            fill(&mut first,&queue,&fence,|s|s);
            assert_eq!(first,[100,100,100,100]);
            match revoke {
                "cancel" => fence.cancel.cancel(),
                "access" => fence.host.lock().access(false),
                "sink" => {
                    let sink=crate::native_media::hold_playback::Sink::new("audio".into(),fence.clone());
                    waltr_desktop::media::OutputSink::revoke(&sink);
                }
                _ => (),
            }
            let mut tail=[0i16;8];
            fill(&mut tail,&queue,&fence,|s|s);
            if revoke != "none" {
                assert_eq!(tail,[0;8]);
                assert_eq!(queue.lock().unwrap().offset,8);
                assert_eq!(queue.lock().unwrap().packets.len(),1);
            } else {
                assert_eq!(tail,[200,200,300,300,400,400,500,500]);
                let now=std::time::Instant::now();
                let delay=std::time::Duration::from_millis(10);
                let duration=buffer_duration(tail.len(),pcm(48000,2));
                assert_eq!(duration,std::time::Duration::from_secs_f64(4.0/48000.0));
                let mut q=queue.lock().unwrap();
                q.scheduled(now,delay,duration,true);
                assert!(!q.drained(now+delay));
                assert!(q.drained(now+delay+duration));
            }
        }
    }

    #[test]
    fn converted_packet_retains_credit_until_final_stereo_frame_and_empty_eof_drains() {
        let (fence, queue) = fixture();
        let credits = Arc::new(tokio::sync::Semaphore::new(4));
        let mut packet = packet(0);
        packet._credit = Some(credits.clone().try_acquire_owned().unwrap());
        packet.bytes = Converter::new(Adaptation::Mono24ToStereo48).packet(bytes(&[10,30])).unwrap();
        { let mut q = queue.lock().unwrap(); q.packets.push_back(packet); q.ended = true; }
        let mut prefix = [0i16;6];
        fill(&mut prefix,&queue,&fence,|s|s);
        assert_eq!(prefix,[10,10,10,10,20,20]);
        assert_eq!(credits.available_permits(),3);
        let mut last = [0i16;2];
        fill(&mut last,&queue,&fence,|s|s);
        assert_eq!(last,[30,30]);
        assert_eq!(credits.available_permits(),4);
        assert!(queue.lock().unwrap().packets.is_empty());

        let (fence, empty) = fixture();
        empty.lock().unwrap().ended = true;
        let mut silence = [7i16;4];
        fill(&mut silence,&empty,&fence,|s|s);
        assert_eq!(silence,[0;4]);
        let now = std::time::Instant::now();
        let duration = buffer_duration(silence.len(),pcm(48000,2));
        let mut q = empty.lock().unwrap();
        q.scheduled(now,std::time::Duration::ZERO,duration,false);
        assert!(!q.drained(now));
        assert!(q.drained(now+duration));
    }
    #[test]
    fn hold_callback_checks_cancel_generation_and_access_before_pcm() {
        for revoke in ["cancel", "generation", "access", "retire", "credential", "sink"] {
            let (base, queue) = fixture();
            let fence = crate::native_media::hold_playback::Fence {
                host: base.audio.clone(), generation: base.generation,
                generation_cancel: tokio_util::sync::CancellationToken::new(),
                cancel: tokio_util::sync::CancellationToken::new(),
                revoked: tokio_util::sync::CancellationToken::new(),
                credential:Some((base.credentials.clone(), base.revision)),
            };
            {
                let mut q = queue.lock().unwrap();
                q.active = true;
                q.packets.push_back(packet(123));
                q.packets.push_back(packet(456));
            }
            let mut data = [0i16; 1];
            fill(&mut data, &queue, &fence, |v| v);
            assert_eq!(data, [123]);
            match revoke {
                "cancel" => fence.cancel.cancel(),
                "generation" => fence.host.lock().generation += 1,
                "access" => fence.host.lock().allowed = false,
                "credential" => { let _ = base.credentials.clear_bearer_token(); }
                "sink" => {
                    let sink = crate::native_media::hold_playback::Sink::new("audio".into(), fence.clone());
                    waltr_desktop::media::OutputSink::revoke(&sink);
                    assert!(!fence.cancel.is_cancelled());
                    assert!(!fence.generation_cancel.is_cancelled());
                }
                _ => fence.generation_cancel.cancel(),
            }
            fill(&mut data, &queue, &fence, |v| v);
            assert_eq!(data, [0], "{revoke}");
            assert_eq!(queue.lock().unwrap().packets.len(), 1, "{revoke}");
        }
    }
    #[tokio::test]
    async fn f4_cancelled_finish_wait_retains_thread_until_drain_or_cancel_join() {
        for cancel in [false, true] {
            let (channel, _receiver) = mpsc::channel(5);
            let stop = tokio_util::sync::CancellationToken::new();
            let stopping = stop.clone();
            let (done, complete) = oneshot::channel();
            let (release, drain) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::spawn(move || {
                while !stopping.is_cancelled() && drain.try_recv().is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let _sent = done.send(());
                Ok(())
            });
            let mut owner = Owner {
                sender: Sender {
                    channel,
                    credits: Arc::new(tokio::sync::Semaphore::new(4)),
                },
                stop,
                thread: Some(thread),
                complete,
            };
            {
                let finish = owner.finish(true);
                tokio::pin!(finish);
                assert!(futures_util::poll!(&mut finish).is_pending());
            }
            assert!(owner.thread.is_some());
            if !cancel {
                release.send(()).unwrap();
            }
            owner.finish(!cancel).await.unwrap();
            assert!(owner.thread.is_none());
        }
    }
    fn fixture() -> (Fence, Arc<Mutex<Queue>>) {
        let audio = Arc::new(crate::native_audio::Host::new(
            crate::activity::ActivityTracker::default(),
        ));
        {
            let mut state = audio.lock();
            state.bind(1);
            state.access(true);
        }
        let credentials = waltr_client::NativeCredentialProvider::new();
        (
            Fence {
                generation: 1,
                epoch: 0,
                current: None,
                audio,
                revision: credentials.credential_revision(),
                credentials,
                cancel: tokio_util::sync::CancellationToken::new(),
            },
            Arc::new(Mutex::new(Queue {
                packets: std::collections::VecDeque::new(),
                offset: 0,
                failed: false,
                active: false,
                ended: false,
                scheduled_until: None,
                last_callback: std::time::Instant::now(),
            })),
        )
    }
    fn packet(value: i16) -> Packet {
        let (done, _receipt) = oneshot::channel();
        Packet {
            bytes: value.to_le_bytes().to_vec(),
            done,
            _credit: None,
        }
    }
    #[test]
    fn f4_priming_is_not_an_underrun_but_active_gaps_are_failure() {
        let (fence, queue) = fixture();
        let mut samples = [9i16; 2];
        fill(&mut samples, &queue, &fence, |s| s);
        assert_eq!(samples, [0, 0]);
        assert!(!queue.lock().unwrap().failed);
        queue.lock().unwrap().packets.extend([packet(1), packet(2)]);
        fill(&mut samples, &queue, &fence, |s| s);
        assert_eq!(samples, [1, 2]);
        assert!(!queue.lock().unwrap().failed);
        fill(&mut samples, &queue, &fence, |s| s);
        assert!(queue.lock().unwrap().failed);
    }
    #[test]
    fn f4_completion_retains_the_stream_until_last_scheduled_device_buffer() {
        let (fence, queue) = fixture();
        {
            let mut q = queue.lock().unwrap();
            q.packets.extend([packet(1), packet(2)]);
            q.ended = true;
        }
        let mut samples = [0i16; 4];
        fill(&mut samples, &queue, &fence, |s| s);
        assert_eq!(samples, [1, 2, 0, 0]);
        let now = std::time::Instant::now();
        let mut q = queue.lock().unwrap();
        q.scheduled(
            now,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(10),
            true,
        );
        assert!(!q.failed);
        assert!(!q.drained(now));
        assert!(!q.drained(now + std::time::Duration::from_millis(29)));
        assert!(q.drained(now + std::time::Duration::from_millis(30)));
        q.scheduled(
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::ZERO,
            true,
        );
        assert!(q.failed);
    }
    #[test]
    fn f1_f4_same_generation_revocation_silences_pending_pcm_without_consumption() {
        let (fence, queue) = fixture();
        {
            let mut q = queue.lock().unwrap();
            q.packets.extend([packet(1), packet(2)]);
            q.active = true;
        }
        fence.audio.lock().access(false);
        let mut samples = [9i16; 2];
        fill(&mut samples, &queue, &fence, |s| s);
        assert_eq!(samples, [0, 0]);
        assert_eq!(queue.lock().unwrap().packets.len(), 2);
        fence.audio.lock().access(true);
        fill(&mut samples, &queue, &fence, |s| s);
        assert_eq!(samples, [0, 0]);
        {
            let mut state = fence.audio.lock();
            state.bind(2);
            state.access(true);
        }
        assert!(!fence.valid());
        let replacement = Fence {
            generation: 2,
            epoch: 0,
            cancel: tokio_util::sync::CancellationToken::new(),
            ..fence.clone()
        };
        assert!(replacement.valid());
        let (_, fresh) = fixture();
        {
            let mut q = fresh.lock().unwrap();
            q.packets.extend([packet(3), packet(4)]);
            q.active = true;
        }
        fill(&mut samples, &fresh, &replacement, |s| s);
        assert_eq!(samples, [3, 4]);
    }
    #[test]
    fn fake_callback_applies_pcm_then_fences_the_next_effect_batch() {
        let audio = Arc::new(crate::native_audio::Host::new(
            crate::activity::ActivityTracker::default(),
        ));
        {
            let mut state = audio.lock();
            state.bind(1);
            state.access(true);
        }
        let credentials = waltr_client::NativeCredentialProvider::new();
        let fence = Fence {
            generation: 1,
            epoch: 0,
            current: None,
            audio,
            revision: credentials.credential_revision(),
            credentials,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let (done, mut receipt) = oneshot::channel();
        let queue = Arc::new(Mutex::new(Queue {
            packets: std::collections::VecDeque::from([Packet {
                bytes: vec![1, 0, 2, 0],
                done,
                _credit: None,
            }]),
            offset: 0,
            failed: false,
            active: true,
            ended: false,
            scheduled_until: None,
            last_callback: std::time::Instant::now(),
        }));
        let mut first = [0i16; 1];
        fill(&mut first, &queue, &fence, |s| s);
        assert_eq!(first, [1]);
        assert_eq!(queue.lock().unwrap().offset, 2);
        fence.cancel.cancel();
        let mut next = [9i16; 1];
        fill(&mut next, &queue, &fence, |s| s);
        assert_eq!(next, [0]);
        assert_eq!(queue.lock().unwrap().offset, 2);
        assert!(matches!(
            receipt.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn cancelled_device_callback_writes_silence_without_consuming_pcm() {
        let audio = Arc::new(crate::native_audio::Host::new(
            crate::activity::ActivityTracker::default(),
        ));
        let credentials = waltr_client::NativeCredentialProvider::new();
        let fence = Fence {
            generation: 1,
            epoch: 0,
            current: None,
            audio,
            revision: credentials.credential_revision(),
            credentials,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let (done, _receipt) = oneshot::channel();
        let queue = Arc::new(Mutex::new(Queue {
            packets: std::collections::VecDeque::from([Packet {
                bytes: vec![1, 2],
                done,
                _credit: None,
            }]),
            offset: 0,
            failed: false,
            active: true,
            ended: false,
            scheduled_until: None,
            last_callback: std::time::Instant::now(),
        }));
        let mut output = [1i16; 16];
        fill(&mut output, &queue, &fence, |s| s);
        assert_eq!(output, [0; 16]);
        assert_eq!(queue.lock().unwrap().offset, 0);
    }
}
