//! Duration-equivalent synthetic PCM, without device access or wall-clock waits.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use waltr_audio_io::{Config, Terminal};
use waltr_desktop::capture::{BACKLOG_SECONDS, CaptureWorker, Error, Factory, Source};

struct Generated {
    remaining: usize,
    next: u16,
    chunk: usize,
    joined: Arc<AtomicBool>,
}
struct GeneratedFactory {
    bytes: usize,
    chunk: usize,
    joined: Arc<AtomicBool>,
}
impl Factory for GeneratedFactory {
    fn open(&self, _: Config) -> Result<Box<dyn Source>, Error> {
        Ok(Box::new(Generated {
            remaining: self.bytes,
            next: 0,
            chunk: self.chunk,
            joined: self.joined.clone(),
        }))
    }
}
impl Source for Generated {
    fn frame(&mut self) -> Option<Vec<u8>> {
        if self.remaining == 0 {
            return None;
        }
        let count = self.remaining.min(self.chunk);
        self.remaining -= count;
        let mut frame = Vec::with_capacity(count);
        for _ in 0..count / 2 {
            frame.extend(self.next.to_le_bytes());
            self.next = self.next.wrapping_add(1);
        }
        Some(frame)
    }
    fn terminal(&self) -> Terminal {
        Terminal::Completed
    }
    fn stop(&self) {}
    fn shutdown(self: Box<Self>) {
        self.joined.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn exact_thirty_second_capacity_preserves_every_sample_even_for_two_byte_fragments() {
    for (rate, chunk) in [(8_000, 2), (24_000, 2), (48_000, 1500)] {
        let mut config = Config::default();
        config.target_rate = rate;
        config.limits.max_chunk_bytes = 1500;
        let capacity = rate as usize * 2 * BACKLOG_SECONDS;
        let joined = Arc::new(AtomicBool::new(false));
        let mut worker = CaptureWorker::start(
            Arc::new(GeneratedFactory {
                bytes: capacity,
                chunk,
                joined: joined.clone(),
            }),
            config,
        );
        assert_eq!(worker.join().await, Ok(()));
        assert!(joined.load(Ordering::SeqCst));
        let mut count = 0usize;
        while let Some(frame) = worker.frames.recv().await {
            assert!(frame.len() <= 1500);
            for sample in frame.chunks_exact(2) {
                assert_eq!(u16::from_le_bytes([sample[0], sample[1]]), count as u16);
                count += 1;
            }
        }
        assert_eq!(count * 2, capacity);
    }
}

#[tokio::test]
async fn cancellation_discards_an_already_captured_backlog() {
    let joined = Arc::new(AtomicBool::new(false));
    let mut worker = CaptureWorker::start(
        Arc::new(GeneratedFactory {
            bytes: 48_000,
            chunk: 1500,
            joined: joined.clone(),
        }),
        Config::default(),
    );
    assert_eq!(worker.join().await, Ok(()));
    worker.cancel();
    assert_eq!(worker.frames.recv().await, None);
    assert!(joined.load(Ordering::SeqCst));
}

#[tokio::test]
async fn one_sample_over_capacity_is_explicit_overflow_not_silent_drop_or_cancellation() {
    let config = Config::default();
    let capacity = config.target_rate as usize * 2 * BACKLOG_SECONDS;
    let joined = Arc::new(AtomicBool::new(false));
    let mut worker = CaptureWorker::start(
        Arc::new(GeneratedFactory {
            bytes: capacity + 2,
            chunk: 1500,
            joined: joined.clone(),
        }),
        config,
    );
    assert_eq!(worker.join().await, Err(Error::Overflow));
    assert!(joined.load(Ordering::SeqCst));
}
