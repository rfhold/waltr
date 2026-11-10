//! Sequential quarantine uploads, single-consumer downloads and durable UTF-8 results.
#![allow(missing_docs)]
use super::split::*;
use super::*;

#[derive(Clone)]
pub struct MediaTransfers {
    pub(crate) rpc: MediaRpc,
    pub(crate) stream: MediaStreamRpc,
}
impl std::fmt::Debug for MediaTransfers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaTransfers")
    }
}
impl MediaTransfers {
    pub async fn read_output(&self, grant: BinaryOutputGrant) -> Result<BlobReader, ClientError> {
        let invocation = grant.0.invocation_id.clone();
        self.open_reader(wire::OpenBlobReadRequest {
            invocation_id: invocation,
            access: Some(wire::open_blob_read_request::Access::OutputGrant(grant.0)),
        })
        .await
    }
    pub async fn read_input(
        &self,
        invocation: String,
        blob: String,
    ) -> Result<BlobReader, ClientError> {
        identity(&invocation, 256)?;
        identity(&blob, 256)?;
        self.open_reader(wire::OpenBlobReadRequest {
            invocation_id: invocation,
            access: Some(wire::open_blob_read_request::Access::InputBlobId(blob)),
        })
        .await
    }
    async fn open_reader(
        &self,
        request: wire::OpenBlobReadRequest,
    ) -> Result<BlobReader, ClientError> {
        self.open_reader_observed(request, None).await
    }
    async fn open_reader_observed(
        &self,
        request: wire::OpenBlobReadRequest,
        observed: Option<std::rc::Rc<std::cell::RefCell<Option<ReaderClose>>>>,
    ) -> Result<BlobReader, ClientError> {
        let invocation = request.invocation_id.clone();
        let (expected_blob, output) = match request.access.as_ref().ok_or_else(invalid)? {
            wire::open_blob_read_request::Access::InputBlobId(id) => (id.clone(), false),
            wire::open_blob_read_request::Access::OutputGrant(grant) => {
                (grant.blob_id.clone(), true)
            }
        };
        let r = match (self.rpc)(MediaRequest::OpenRead(request)).await? {
            MediaResponse::OpenRead(r) => r,
            _ => return Err(invalid()),
        };
        let credentials = r.credentials.ok_or_else(invalid)?;
        if credentials.invocation_id != invocation
            || credentials.generation.is_empty()
            || credentials.reader_id.is_empty()
            || !(32..=64).contains(&credentials.secret.len())
        {
            return Err(invalid());
        }
        if let Some(slot) = observed {
            *slot.borrow_mut() = Some(ReaderClose {
                credentials: credentials.clone(),
                rpc: self.rpc.clone(),
            });
        }
        let descriptor = crate::transport::routine::blob(r.blob.ok_or_else(invalid)?)?;
        if descriptor.blob_id != expected_blob
            || (output
                && !descriptor
                    .remaining_validity_ms
                    .is_some_and(|ms| ms.0 > 0 && ms.0 <= 900000))
            || (!output && descriptor.remaining_validity_ms.is_some())
        {
            return Err(invalid());
        }
        let source = (self.stream)(MediaStreamRequest::Blob(wire::ReadBlobRequest {
            credentials: Some(credentials.clone()),
            max_bytes: 65536,
        }))
        .await?;
        Ok(BlobReader {
            descriptor,
            credentials,
            rpc: self.rpc.clone(),
            source,
            next: 0,
            bytes: 0,
            eof: false,
            clean: false,
            failed: false,
        })
    }
    pub fn text_reader(
        &self,
        invocation: String,
        descriptors: &[crate::RoutineTextResult],
        selected: &str,
    ) -> Result<DurableTextReader, ClientError> {
        identity(&invocation, 256)?;
        let total = descriptors.iter().try_fold(0u64, |sum, d| {
            sum.checked_add(d.encoded_charge.0).ok_or_else(invalid)
        })?;
        if descriptors.len() > 32 || total > 8 * 1024 * 1024 {
            return Err(invalid());
        }
        let d = descriptors
            .iter()
            .find(|d| d.id == selected)
            .ok_or_else(invalid)?;
        if d.utf8_bytes.0 > 1024 * 1024 {
            return Err(invalid());
        }
        Ok(DurableTextReader {
            rpc: self.rpc.clone(),
            invocation,
            result: d.id.clone(),
            offset: 0,
            size: d.utf8_bytes.0,
            pending: vec![],
            done: false,
            poisoned: false,
        })
    }
}

#[derive(Clone)]
struct ReaderClose {
    credentials: wire::BlobReaderCredentials,
    rpc: MediaRpc,
}
impl ReaderClose {
    async fn close(&self) -> Result<(), ClientError> {
        match (self.rpc)(MediaRequest::CloseRead(wire::CloseBlobReadRequest {
            credentials: Some(self.credentials.clone()),
        }))
        .await?
        {
            MediaResponse::Unit => Ok(()),
            _ => Err(invalid()),
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod owned {
    use super::*;
    use futures_channel::{mpsc, oneshot};
    use futures_util::{
        FutureExt,
        future::{Either, FusedFuture, LocalBoxFuture, Shared, select},
    };
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    static READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct ReaderCredit;
    impl ReaderCredit {
        fn reserve(limit: u32) -> Result<Self, ClientError> {
            READERS
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |count| (count < limit as usize).then_some(count + 1),
                )
                .map_err(|_| ClientError::new("media reader capacity"))?;
            Ok(Self)
        }
    }
    impl Drop for ReaderCredit {
        fn drop(&mut self) {
            READERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct Stop(Rc<StopState>);
    #[derive(Default)]
    struct StopState {
        cancelled: Cell<bool>,
        wake: futures_util::task::AtomicWaker,
    }
    impl Stop {
        pub(crate) fn cancel(&self) {
            self.0.cancelled.set(true);
            self.0.wake.wake();
        }
        pub(crate) fn is_cancelled(&self) -> bool {
            self.0.cancelled.get()
        }
        pub(crate) async fn cancelled(&self) {
            futures_util::future::poll_fn(|cx| {
                self.0.wake.register(cx.waker());
                if self.is_cancelled() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        pub(crate) async fn race<T>(
            &self,
            deadline: Pin<&mut dyn Future<Output = ()>>,
            work: impl Future<Output = T>,
        ) -> Result<T, ClientError> {
            let stop = self.cancelled();
            futures_util::pin_mut!(stop, work);
            match select(select(stop, deadline), work).await {
                Either::Right((result, _)) => Ok(result),
                _ => {
                    self.cancel();
                    Err(ClientError::new("media owner stopped or expired"))
                }
            }
        }
    }
    struct Wait {
        stop: Stop,
        busy: Rc<Cell<bool>>,
        complete: bool,
    }
    impl Wait {
        fn new(stop: Stop, busy: Rc<Cell<bool>>) -> Result<Self, ClientError> {
            if busy.replace(true) {
                return Err(ClientError::new("media reader busy"));
            }
            Ok(Self {
                stop,
                busy,
                complete: false,
            })
        }
    }
    impl Drop for Wait {
        fn drop(&mut self) {
            self.busy.set(false);
            if !self.complete {
                self.stop.cancel();
            }
        }
    }
    type ReadReply = oneshot::Sender<Result<Option<Vec<u8>>, ClientError>>;
    type OpenReply = oneshot::Receiver<Result<crate::RoutineBlobDescriptor, ClientError>>;
    pub(crate) struct DownloadDeadline {
        timer: LocalBoxFuture<'static, ()>,
        expired: Rc<dyn Fn() -> bool>,
    }
    impl DownloadDeadline {
        pub(crate) fn new(
            timer: LocalBoxFuture<'static, ()>,
            expired: impl Fn() -> bool + 'static,
        ) -> Self {
            Self {
                timer,
                expired: Rc::new(expired),
            }
        }
    }
    #[derive(Clone)]
    pub(crate) struct Download {
        pub(crate) stop: Stop,
        pub(crate) finished: Rc<Cell<bool>>,
        pub(crate) verified: Rc<Cell<bool>>,
        busy: Rc<Cell<bool>>,
        opened: Rc<RefCell<Option<OpenReply>>>,
        requests: Rc<RefCell<mpsc::Sender<ReadReply>>>,
        joined: Shared<LocalBoxFuture<'static, bool>>,
        expired: Rc<dyn Fn() -> bool>,
    }
    impl Download {
        pub(crate) fn start(
            transfers: MediaTransfers,
            grant: BinaryOutputGrant,
            deadline: DownloadDeadline,
            close_timeout: impl FnOnce() -> LocalBoxFuture<'static, ()> + 'static,
            reader_limit: u32,
        ) -> Result<(Self, LocalBoxFuture<'static, ()>), ClientError> {
            let credit = ReaderCredit::reserve(reader_limit)?;
            let stop = Stop::default();
            let finished = Rc::new(Cell::new(false));
            let verified_close = Rc::new(Cell::new(false));
            let (opened, opening) = oneshot::channel();
            let (requests, mut commands) = mpsc::channel::<ReadReply>(1);
            let (joined, joining) = oneshot::channel();
            let owner = Self {
                stop: stop.clone(),
                finished: finished.clone(),
                verified: verified_close.clone(),
                busy: Rc::new(Cell::new(false)),
                opened: Rc::new(RefCell::new(Some(opening))),
                requests: Rc::new(RefCell::new(requests)),
                joined: joining.map(|r| r.unwrap_or(false)).boxed_local().shared(),
                expired: deadline.expired.clone(),
            };
            let driver = async move {
                let _credit = credit;
                let mut timer = Box::pin(deadline.timer.fuse());
                let authority = Rc::new(RefCell::new(None));
                let invocation = grant.0.invocation_id.clone();
                let checkout = transfers.open_reader_observed(
                    wire::OpenBlobReadRequest {
                        invocation_id: invocation,
                        access: Some(wire::open_blob_read_request::Access::OutputGrant(grant.0)),
                    },
                    Some(authority.clone()),
                );
                if (deadline.expired)() {
                    stop.cancel();
                }
                let reader = stop.race(timer.as_mut(), checkout).await.and_then(|r| r);
                match reader {
                    Ok(mut reader) => {
                        if opened.send(Ok(reader.descriptor.clone())).is_err() {
                            stop.cancel();
                        }
                        loop {
                            let reply = match stop.race(timer.as_mut(), commands.next()).await {
                                Ok(Some(reply)) => reply,
                                _ => break,
                            };
                            let next = stop.race(timer.as_mut(), reader.next()).await;
                            let result = next.map(|r| r.transpose()).and_then(|r| r);
                            let end = !matches!(&result, Ok(Some(_)));
                            if reply.send(result).is_err() {
                                stop.cancel();
                            }
                            if end {
                                break;
                            }
                        }
                        drop(reader);
                    }
                    Err(error) => {
                        drop(opened.send(Err(error)));
                    }
                }
                commands.close();
                while let Some(reply) = commands.next().await {
                    drop(reply.send(Err(invalid())));
                }
                drop(commands);
                let close = authority.borrow().clone();
                let verified = if let Some(close) = close {
                    let call = close.close();
                    let close_timeout = close_timeout();
                    futures_util::pin_mut!(call, close_timeout);
                    matches!(select(call, close_timeout).await, Either::Left((Ok(()), _)))
                } else {
                    false
                };
                verified_close.set(verified);
                if verified {
                    finished.set(true);
                }
                let _joined = joined.send(verified);
                if !verified && !timer.is_terminated() {
                    // Quarantine retains close authority and its slot until the original expiry.
                    timer.await;
                }
                drop(authority.borrow_mut().take());
                finished.set(true);
            }
            .boxed_local();
            Ok((owner, driver))
        }
        pub(crate) fn ensure_live(&self) -> Result<(), ClientError> {
            if (self.expired)() {
                self.stop.cancel();
            }
            if self.stop.is_cancelled() {
                Err(invalid())
            } else {
                Ok(())
            }
        }
        pub(crate) fn ensure_descriptor_live(&self, remaining_ms: u64) -> Result<(), ClientError> {
            if remaining_ms == 0 {
                self.stop.cancel();
            }
            self.ensure_live()
        }
        pub(crate) async fn opened(&self) -> Result<crate::RoutineBlobDescriptor, ClientError> {
            let mut wait = Wait::new(self.stop.clone(), self.busy.clone())?;
            let receiver = self.opened.borrow_mut().take().ok_or_else(invalid)?;
            let result = receiver.await.map_err(|_| invalid())?;
            wait.complete = true;
            self.ensure_live()?;
            result
        }
        pub(crate) async fn next(&self) -> Result<Option<Vec<u8>>, ClientError> {
            let mut wait = Wait::new(self.stop.clone(), self.busy.clone())?;
            self.ensure_live()?;
            if self.finished.get() {
                return Err(invalid());
            }
            let (sender, receiver) = oneshot::channel();
            self.requests
                .borrow_mut()
                .try_send(sender)
                .map_err(|_| invalid())?;
            let result = receiver.await.map_err(|_| invalid())?;
            wait.complete = true;
            self.ensure_live()?;
            result
        }
        pub(crate) async fn close(&self) -> bool {
            self.stop.cancel();
            self.joined.clone().await
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        struct Dropped(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        struct Never(Dropped);
        impl Stream for Never {
            type Item = Result<MediaStreamResponse, ClientError>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                let _keep = &self.0;
                Poll::Pending
            }
        }
        fn transfers(
            mode: &'static str,
            drops: Arc<std::sync::atomic::AtomicUsize>,
            calls: Rc<Cell<usize>>,
        ) -> MediaTransfers {
            let stream_drops = drops.clone();
            let rpc: MediaRpc = Arc::new(move |request| {
                let drops = drops.clone();
                let calls = calls.clone();
                async move {
                    calls.set(calls.get() + 1);
                    match request {
                        MediaRequest::OpenRead(_) => {
                            if mode == "open" {
                                let _drop = Dropped(drops);
                                return futures_util::future::pending().await;
                            }
                            if mode == "error" {
                                return Err(invalid());
                            }
                            Ok(MediaResponse::OpenRead(wire::OpenBlobReadResponse {
                                credentials: Some(wire::BlobReaderCredentials {
                                    invocation_id: "inv".into(),
                                    reader_id: "reader".into(),
                                    generation: "gen".into(),
                                    secret: vec![7; 32],
                                }),
                                blob: Some(wire::BlobDescriptor {
                                    blob_id: "blob".into(),
                                    metadata: Some(wire::BlobMetadata {
                                        display_name: "file".into(),
                                        content_type: "application/octet-stream".into(),
                                        bytes: 1,
                                        sha256: vec![3; 32],
                                        image: None,
                                    }),
                                    remaining_validity_ms: Some(900000),
                                }),
                            }))
                        }
                        MediaRequest::CloseRead(r) => {
                            assert_eq!(r.credentials.unwrap().reader_id, "reader");
                            if mode == "close" {
                                let _drop = Dropped(drops);
                                futures_util::future::pending().await
                            } else {
                                Ok(MediaResponse::Unit)
                            }
                        }
                        _ => panic!("unexpected RPC"),
                    }
                }
                .boxed_local()
            });
            let stream: MediaStreamRpc = Arc::new(move |_| {
                let stream_drops = stream_drops.clone();
                async move {
                    if mode == "stream" {
                        let _drop = Dropped(stream_drops);
                        return futures_util::future::pending().await;
                    }
                    if mode == "ready" {
                        return Ok(Box::pin(futures_util::stream::iter([Ok(
                            MediaStreamResponse::Blob(wire::ReadBlobResponse {
                                sequence: 0,
                                data: vec![42],
                                eof: true,
                            }),
                        )])) as MediaStream);
                    }
                    Ok(Box::pin(Never(Dropped(stream_drops))) as MediaStream)
                }
                .boxed_local()
            });
            MediaTransfers { rpc, stream }
        }
        fn grant() -> BinaryOutputGrant {
            BinaryOutputGrant(wire::BinaryOutputCredentials {
                invocation_id: "inv".into(),
                port: "file".into(),
                blob_id: "blob".into(),
                generation: "gen".into(),
                secret: vec![9; 32],
            })
        }
        async fn stopped_download(mode: &'static str, expiry: bool, drop_waiter: bool) {
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls = Rc::new(Cell::new(0));
            let (expire, expired) = oneshot::channel();
            let (owner, mut driver) = Download::start(
                transfers(mode, drops.clone(), calls.clone()),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = expired.await;
                    }
                    .boxed_local(),
                    || false,
                ),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            let mut opening = Box::pin(owner.opened());
            assert!(opening.as_mut().now_or_never().is_none());
            assert!(driver.as_mut().now_or_never().is_none());
            if mode == "read" {
                assert!(opening.await.is_ok());
                let mut read = Box::pin(owner.next());
                assert!(read.as_mut().now_or_never().is_none());
                assert!(driver.as_mut().now_or_never().is_none());
                for _ in 0..1000 {
                    assert!(owner.next().now_or_never().unwrap().is_err());
                }
                if drop_waiter {
                    drop(read);
                } else {
                    if expiry {
                        expire.send(()).unwrap();
                    } else {
                        owner.stop.cancel();
                    }
                    assert!(driver.as_mut().now_or_never().is_some());
                    assert!(read.await.is_err());
                    assert!(owner.close().await);
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                    return;
                }
            } else if drop_waiter {
                drop(opening);
            } else {
                if expiry {
                    expire.send(()).unwrap();
                } else {
                    owner.stop.cancel();
                }
                let completed = driver.as_mut().now_or_never().is_some();
                assert!(opening.await.is_err());
                if mode == "open" {
                    assert!(!owner.close().await);
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                } else {
                    assert!(owner.close().await);
                    assert!(completed);
                }
                return;
            }
            let _ = driver.as_mut().now_or_never();
            assert_eq!(owner.close().await, mode != "open");
            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        #[tokio::test]
        async fn pending_checkout_cancel_wakes_waiter_without_remote_proof() {
            stopped_download("open", false, false).await;
        }
        #[tokio::test]
        async fn pending_checkout_expiry_drops_rpc_without_renewing_lease() {
            stopped_download("open", true, false).await;
        }
        #[tokio::test]
        async fn pending_stream_admission_cancel_retains_exact_close_authority() {
            stopped_download("stream", false, false).await;
        }
        #[tokio::test]
        async fn pending_read_cancel_wakes_without_reader_mutex() {
            stopped_download("read", false, false).await;
        }
        #[tokio::test]
        async fn pending_read_pinned_expiry_closes_original_reader() {
            stopped_download("read", true, false).await;
        }
        #[tokio::test]
        async fn dropped_checkout_waiter_keeps_driver_close_authority() {
            stopped_download("open", false, true).await;
        }
        #[tokio::test]
        async fn dropped_read_waiter_keeps_driver_close_authority() {
            stopped_download("read", false, true).await;
        }
        #[tokio::test]
        async fn stalled_close_reports_uncertainty_and_quarantines_until_original_expiry() {
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls = Rc::new(Cell::new(0));
            let (expire, expired) = oneshot::channel();
            let (timeout, timed_out) = oneshot::channel();
            let (owner, mut driver) = Download::start(
                transfers("close", drops.clone(), calls),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = expired.await;
                    }
                    .boxed_local(),
                    || false,
                ),
                || {
                    async move {
                        let _timeout = timed_out.await;
                    }
                    .boxed_local()
                },
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            owner.opened().await.unwrap();
            owner.stop.cancel();
            assert!(driver.as_mut().now_or_never().is_none());
            timeout.send(()).unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            assert!(!owner.close().await);
            assert!(!owner.finished.get());
            assert!(!owner.verified.get());
            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 2);
            expire.send(()).unwrap();
            driver.await;
            assert!(owner.finished.get());
        }
        #[tokio::test]
        async fn rejected_checkout_never_retries_and_expiry_releases_quarantine() {
            let calls = Rc::new(Cell::new(0));
            let (tx, rx) = oneshot::channel();
            let (owner, mut driver) = Download::start(
                transfers(
                    "error",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    calls.clone(),
                ),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = rx.await;
                    }
                    .boxed_local(),
                    || false,
                ),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            assert!(owner.opened().await.is_err());
            assert_eq!(calls.get(), 1);
            assert!(!owner.close().await);
            tx.send(()).unwrap();
            driver.await;
            assert_eq!(calls.get(), 1);
        }
        #[tokio::test]
        async fn admission_cancel_drops_never_rpc_and_wins_ready_ack() {
            let stop = Stop::default();
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let tracked = Dropped(drops.clone());
            let mut deadline = Box::pin(futures_util::future::pending());
            let mut opening = Box::pin(stop.race(deadline.as_mut(), async move {
                let _tracked = tracked;
                futures_util::future::pending::<()>().await
            }));
            assert!(opening.as_mut().now_or_never().is_none());
            stop.cancel();
            assert!(opening.await.is_err());
            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                stop.race(deadline.as_mut(), async { "late ack" })
                    .await
                    .is_err()
            );
            let replacement = Stop::default();
            assert_eq!(
                replacement
                    .race(deadline.as_mut(), async { "new gesture" })
                    .await
                    .unwrap(),
                "new gesture"
            );
        }
        #[tokio::test]
        async fn admission_absolute_deadline_is_not_restarted_between_open_and_accepted() {
            let stop = Stop::default();
            let (expire, expired) = oneshot::channel();
            let mut deadline = async move {
                let _expired = expired.await;
            }
            .boxed_local();
            assert_eq!(
                stop.race(deadline.as_mut(), async { "open ack" })
                    .await
                    .unwrap(),
                "open ack"
            );
            let mut accepted =
                Box::pin(stop.race(deadline.as_mut(), futures_util::future::pending::<()>()));
            assert!(accepted.as_mut().now_or_never().is_none());
            expire.send(()).unwrap();
            assert!(accepted.await.is_err());
        }
        #[tokio::test]
        async fn late_checkout_cancel_never_publishes_descriptor_or_replaces_owner() {
            let (expire, expired) = oneshot::channel::<()>();
            let (owner, mut driver) = Download::start(
                transfers(
                    "read",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    Rc::new(Cell::new(0)),
                ),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = expired.await;
                    }
                    .boxed_local(),
                    || false,
                ),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            owner.stop.cancel();
            assert!(owner.opened().await.is_err());
            assert!(driver.as_mut().now_or_never().is_some());
            assert!(owner.close().await);
            drop(expire);
        }
        #[tokio::test]
        async fn cancellation_registers_and_wakes_actual_pending_driver() {
            struct Wakes(std::sync::atomic::AtomicUsize);
            impl futures_util::task::ArcWake for Wakes {
                fn wake_by_ref(this: &Arc<Self>) {
                    this.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let wakes = Arc::new(Wakes(std::sync::atomic::AtomicUsize::new(0)));
            let waker = futures_util::task::waker_ref(&wakes);
            let mut cx = Context::from_waker(&waker);
            let stop = Stop::default();
            let mut deadline = Box::pin(futures_util::future::pending());
            let mut driver =
                Box::pin(stop.race(deadline.as_mut(), futures_util::future::pending::<()>()));
            assert!(driver.as_mut().poll(&mut cx).is_pending());
            stop.cancel();
            assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) > 0);
            assert!(driver.await.is_err());
        }
        #[tokio::test]
        async fn ready_download_bytes_are_not_published_after_cancel() {
            let (expire, expired) = oneshot::channel::<()>();
            let (owner, mut driver) = Download::start(
                transfers(
                    "ready",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    Rc::new(Cell::new(0)),
                ),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = expired.await;
                    }
                    .boxed_local(),
                    || false,
                ),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            owner.opened().await.unwrap();
            let mut read = Box::pin(owner.next());
            assert!(read.as_mut().now_or_never().is_none());
            assert!(driver.as_mut().now_or_never().is_none());
            owner.stop.cancel();
            assert!(read.await.is_err());
            driver.await;
            assert!(owner.close().await);
            drop(expire);
        }
        async fn queued_reply_expiry(bytes: bool, expired: bool) {
            let clock = Rc::new(Cell::new(0u64));
            let observed_clock = clock.clone();
            let calls = Rc::new(Cell::new(0));
            let (owner, mut driver) = Download::start(
                transfers(
                    "ready",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    calls.clone(),
                ),
                grant(),
                DownloadDeadline::new(futures_util::future::pending().boxed_local(), move || {
                    observed_clock.get() >= 10
                }),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            // Checkout queues success; no consumer or timer callback has run.
            assert!(driver.as_mut().now_or_never().is_none());
            assert_eq!(calls.get(), 1);
            if bytes {
                owner.opened().await.unwrap();
                let mut read = Box::pin(owner.next());
                assert!(read.as_mut().now_or_never().is_none());
                assert!(driver.as_mut().now_or_never().is_none());
                clock.set(if expired { 11 } else { 9 });
                assert!(!owner.stop.is_cancelled());
                let result = read
                    .as_mut()
                    .now_or_never()
                    .expect("queued bytes must be ready");
                if expired {
                    assert!(result.is_err());
                } else {
                    assert_eq!(result.unwrap(), Some(vec![42]));
                }
            } else {
                clock.set(if expired { 11 } else { 9 });
                assert!(!owner.stop.is_cancelled());
                let result = owner
                    .opened()
                    .now_or_never()
                    .expect("queued descriptor must be ready");
                if expired {
                    assert!(result.is_err());
                } else {
                    assert_eq!(result.unwrap().blob_id, "blob");
                }
            }
            assert_eq!(owner.stop.is_cancelled(), expired);
            assert!(!owner.finished.get());
            assert!(!owner.verified.get());
            assert_eq!(calls.get(), 1);
            owner.stop.cancel();
            driver.await;
            assert!(owner.finished.get());
            assert!(owner.close().await);
            assert_eq!(calls.get(), 2);
        }
        #[tokio::test]
        async fn queued_descriptor_expiry_rejects_before_driver_or_timer_repoll() {
            queued_reply_expiry(false, true).await;
        }
        #[tokio::test]
        async fn queued_bytes_expiry_rejects_before_driver_or_timer_repoll() {
            queued_reply_expiry(true, true).await;
        }
        #[tokio::test]
        async fn queued_nonexpired_descriptor_still_publishes() {
            queued_reply_expiry(false, false).await;
        }
        #[tokio::test]
        async fn queued_nonexpired_bytes_still_publish() {
            queued_reply_expiry(true, false).await;
        }
        #[tokio::test]
        async fn already_expired_clock_prevents_checkout_dispatch_without_timer_callback() {
            let calls = Rc::new(Cell::new(0));
            let (expire, expired) = oneshot::channel::<()>();
            let (owner, mut driver) = Download::start(
                transfers(
                    "ready",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    calls.clone(),
                ),
                grant(),
                DownloadDeadline::new(
                    async move {
                        let _expired = expired.await;
                    }
                    .boxed_local(),
                    || true,
                ),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            assert_eq!(calls.get(), 0);
            assert!(owner.opened().await.is_err());
            assert!(!owner.close().await);
            assert!(!owner.finished.get());
            expire.send(()).unwrap();
            driver.await;
            assert!(owner.finished.get());
            assert_eq!(calls.get(), 0);
        }
        #[tokio::test]
        async fn zero_ttl_descriptor_boundary_rejects_and_preserves_close_authority() {
            let calls = Rc::new(Cell::new(0));
            let (owner, mut driver) = Download::start(
                transfers(
                    "ready",
                    Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    calls.clone(),
                ),
                grant(),
                DownloadDeadline::new(futures_util::future::pending().boxed_local(), || false),
                || futures_util::future::pending().boxed_local(),
                32,
            )
            .unwrap();
            assert!(driver.as_mut().now_or_never().is_none());
            owner.opened().await.unwrap();
            assert!(owner.ensure_descriptor_live(0).is_err());
            assert!(owner.stop.is_cancelled());
            assert!(!owner.finished.get());
            assert_eq!(calls.get(), 1);
            driver.await;
            assert!(owner.close().await);
            assert_eq!(calls.get(), 2);
        }
    }
}

pub struct BlobUpload {
    queue: Arc<InteractionQueue>,
    rpc: MediaRpc,
    invocation: String,
    port: String,
    declaration: wire::BinaryInputDeclaration,
    upload: Option<String>,
    sequence: u64,
    offset: u64,
    chunk: u32,
    poisoned: bool,
    committed: bool,
}
impl std::fmt::Debug for BlobUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobUpload")
            .field("offset", &self.offset)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}
impl BlobUpload {
    /// Negotiated maximum append size; zero before [`Self::begin`] succeeds.
    #[must_use]
    pub const fn chunk_bytes(&self) -> u32 {
        self.chunk
    }

    pub(crate) fn new(
        rpc: MediaRpc,
        invocation: String,
        port: String,
        declaration: wire::BinaryInputDeclaration,
        queue: Arc<InteractionQueue>,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            queue,
            rpc,
            invocation,
            port,
            declaration,
            upload: None,
            sequence: 0,
            offset: 0,
            chunk: 0,
            poisoned: false,
            committed: false,
        })
    }
    pub async fn begin(&mut self) -> Result<(), ClientError> {
        if !self.queue.permits_upload() {
            return Err(invalid());
        }
        if self.poisoned || self.upload.is_some() || self.committed {
            return Err(invalid());
        }
        let b = &self.declaration;
        let purpose = match &b.purpose {
            Some(wire::binary_input_declaration::Purpose::File(p)) => {
                wire::begin_blob_upload_request::Purpose::File(*p)
            }
            Some(wire::binary_input_declaration::Purpose::Image(i)) => {
                wire::begin_blob_upload_request::Purpose::Image(*i)
            }
            _ => return Err(invalid()),
        };
        self.poisoned = true;
        let mut pending = PendingMediaWait::new(self.queue.clone());
        let r = self
            .queue
            .upload_wait((self.rpc)(MediaRequest::Begin(
                wire::BeginBlobUploadRequest {
                    invocation_id: self.invocation.clone(),
                    port: self.port.clone(),
                    display_name: b.display_name.clone(),
                    content_type: b.content_type.clone(),
                    declared_bytes: b.declared_bytes,
                    expected_sha256: b.expected_sha256.clone(),
                    purpose: Some(purpose),
                },
            )))
            .await?;
        let MediaResponse::Begin(r) = r else {
            return Err(invalid());
        };
        identity(&r.upload_id, 256)?;
        if r.chunk_bytes == 0 || r.chunk_bytes > 65536 {
            return Err(invalid());
        }
        self.upload = Some(r.upload_id);
        self.chunk = r.chunk_bytes;
        self.poisoned = false;
        pending.complete();
        Ok(())
    }
    pub async fn append(&mut self, data: &[u8]) -> Result<(), ClientError> {
        if !self.queue.permits_upload() {
            return Err(invalid());
        }
        if self.poisoned || self.committed || data.is_empty() || data.len() > self.chunk as usize {
            return Err(invalid());
        }
        let next = self
            .offset
            .checked_add(data.len() as u64)
            .ok_or_else(invalid)?;
        if next > self.declaration.declared_bytes.ok_or_else(invalid)? {
            return Err(invalid());
        }
        let upload = self.upload.clone().ok_or_else(invalid)?;
        self.poisoned = true;
        let mut pending = PendingMediaWait::new(self.queue.clone());
        let r = self
            .queue
            .upload_wait((self.rpc)(MediaRequest::Append(
                wire::AppendBlobUploadRequest {
                    invocation_id: self.invocation.clone(),
                    upload_id: upload,
                    sequence: self.sequence,
                    offset: self.offset,
                    data: data.to_vec(),
                },
            )))
            .await?;
        let MediaResponse::Append(r) = r else {
            return Err(invalid());
        };
        if r.accepted_sequence != self.sequence || r.next_offset != next {
            return Err(invalid());
        }
        self.sequence = self.sequence.checked_add(1).ok_or_else(invalid)?;
        self.offset = next;
        self.poisoned = false;
        pending.complete();
        Ok(())
    }
    pub async fn commit(
        &mut self,
    ) -> Result<(crate::RoutineBlobDescriptor, wire::InputEvent), ClientError> {
        if !self.queue.permits_upload() {
            return Err(invalid());
        }
        if self.poisoned || self.committed || Some(self.offset) != self.declaration.declared_bytes {
            return Err(invalid());
        }
        let upload = self.upload.clone().ok_or_else(invalid)?;
        self.poisoned = true;
        let mut pending = PendingMediaWait::new(self.queue.clone());
        let r = self
            .queue
            .upload_wait((self.rpc)(MediaRequest::Commit(
                wire::CommitBlobUploadRequest {
                    invocation_id: self.invocation.clone(),
                    upload_id: upload,
                    next_sequence: self.sequence,
                    bytes: self.offset,
                },
            )))
            .await?;
        let MediaResponse::Commit(r) = r else {
            return Err(invalid());
        };
        let b = crate::transport::routine::blob(r.blob.ok_or_else(invalid)?)?;
        if b.bytes.0 != self.offset
            || b.display_name != self.declaration.display_name
            || b.content_type != self.declaration.content_type
            || self
                .declaration
                .expected_sha256
                .as_ref()
                .is_some_and(|d| d != &b.sha256)
            || b.remaining_validity_ms.is_some()
        {
            return Err(invalid());
        }
        self.committed = true;
        self.poisoned = false;
        pending.complete();
        let event = wire::InputEvent {
            event: Some(input_event::Event::Blob(wire::BlobInput {
                port: self.port.clone(),
                blob_id: b.blob_id.clone(),
            })),
        };
        Ok((b, event))
    }
    pub async fn cancel(&mut self) -> Result<(), ClientError> {
        self.poisoned = true;
        let Some(upload) = self.upload.take() else {
            return Ok(());
        };
        match (self.rpc)(MediaRequest::CancelUpload(wire::CancelBlobUploadRequest {
            invocation_id: self.invocation.clone(),
            upload_id: upload,
        }))
        .await?
        {
            MediaResponse::Unit => Ok(()),
            _ => Err(invalid()),
        }
    }
}
impl Drop for BlobUpload {
    fn drop(&mut self) {
        if !self.committed
            && (self.poisoned || self.upload.is_some())
            && self.queue.permits_upload()
        {
            self.queue.cancel();
        }
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct WakeCount(AtomicUsize);
    impl futures_util::task::ArcWake for WakeCount {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn blocked(phase: u8, finish: bool) {
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (count, drop_count) = (calls.clone(), dropped.clone());
        let rpc: MediaRpc = Arc::new(move |request| {
            let calls = count.clone();
            let dropped = drop_count.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let _drop = Dropped(dropped);
                let response = match request {
                    MediaRequest::Begin(_) => {
                        if phase == 0 {
                            futures_util::future::pending::<()>().await;
                        }
                        MediaResponse::Begin(wire::BeginBlobUploadResponse {
                            upload_id: "upload".into(),
                            chunk_bytes: 7,
                        })
                    }
                    MediaRequest::Append(_) => {
                        if phase == 1 {
                            futures_util::future::pending::<()>().await;
                        }
                        MediaResponse::Append(wire::AppendBlobUploadResponse {
                            accepted_sequence: 0,
                            next_offset: 3,
                        })
                    }
                    MediaRequest::Commit(_) => {
                        futures_util::future::pending::<()>().await;
                        unreachable!()
                    }
                    _ => return Err(invalid()),
                };
                Ok(response)
            }
            .boxed_local()
        });
        let queue = InteractionQueue::new();
        queue.state.lock().unwrap().accepted = true;
        let mut upload = BlobUpload::new(
            rpc,
            "invocation".into(),
            "file".into(),
            wire::BinaryInputDeclaration {
                display_name: "file".into(),
                content_type: "application/octet-stream".into(),
                declared_bytes: Some(3),
                expected_sha256: None,
                purpose: Some(wire::binary_input_declaration::Purpose::File(
                    wire::FilePurpose {},
                )),
            },
            queue.clone(),
        )
        .unwrap();
        assert_eq!(upload.chunk_bytes(), 0);
        if phase > 0 {
            upload.begin().await.unwrap();
            assert_eq!(upload.chunk_bytes(), 7);
        }
        if phase > 1 {
            upload.append(&[1, 2, 3]).await.unwrap();
        }
        let before = dropped.load(Ordering::SeqCst);
        {
            let mut waiter: futures_util::future::LocalBoxFuture<'_, Result<(), ClientError>> =
                match phase {
                    0 => upload.begin().boxed_local(),
                    1 => upload.append(&[1, 2, 3]).boxed_local(),
                    _ => upload.commit().map(|r| r.map(|_| ())).boxed_local(),
                };
            let wake_count = Arc::new(WakeCount(AtomicUsize::new(0)));
            let waker = futures_util::task::waker(wake_count.clone());
            assert!(
                waiter
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
            if finish {
                queue.state.lock().unwrap().input_finished = true;
                queue.wake_uploads();
            } else {
                queue.cancel();
            }
            assert!(wake_count.0.load(Ordering::SeqCst) > 0);
            assert!(waiter.await.is_err());
        }
        assert_eq!(dropped.load(Ordering::SeqCst), before + 1);
        assert!(!queue.state.lock().unwrap().ended);
        assert_eq!(queue.state.lock().unwrap().cancelled, !finish);
        let before = calls.load(Ordering::SeqCst);
        assert!(upload.begin().await.is_err());
        assert!(upload.append(&[1, 2, 3]).await.is_err());
        assert!(upload.commit().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }
    #[tokio::test]
    async fn cancelled_blocked_blob_begin_drops_wait_without_join_proof() {
        blocked(0, false).await;
    }
    #[tokio::test]
    async fn cancelled_blocked_blob_append_drops_wait_without_join_proof() {
        blocked(1, false).await;
    }
    #[tokio::test]
    async fn cancelled_blocked_blob_commit_drops_wait_without_join_proof() {
        blocked(2, false).await;
    }
    #[tokio::test]
    async fn input_finish_wakes_upload_wait_without_cancelling_output_drain() {
        blocked(0, true).await;
    }
    #[test]
    fn finished_input_rejects_upload_without_cancelling_output_drain() {
        let queue = InteractionQueue::new();
        {
            let mut s = queue.state.lock().unwrap();
            s.accepted = true;
            s.input_finished = true;
        }
        let guard = PendingMediaWait::new(queue.clone());
        assert!(!queue.permits_upload());
        drop(guard);
        assert!(!queue.state.lock().unwrap().cancelled);
    }
}

pub struct BlobReader {
    pub descriptor: crate::RoutineBlobDescriptor,
    credentials: wire::BlobReaderCredentials,
    rpc: MediaRpc,
    source: MediaStream,
    next: u64,
    bytes: u64,
    eof: bool,
    clean: bool,
    failed: bool,
}
impl std::fmt::Debug for BlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobReader")
            .field("clean", &self.clean)
            .finish_non_exhaustive()
    }
}
impl BlobReader {
    fn stop(&mut self) {
        self.source = Box::pin(futures_util::stream::empty());
        self.failed = true;
    }
    pub fn complete(&self) -> bool {
        self.clean && !self.failed
    }
    pub async fn close(&mut self) -> Result<(), ClientError> {
        self.source = Box::pin(futures_util::stream::empty());
        self.failed = !self.clean;
        ReaderClose {
            credentials: self.credentials.clone(),
            rpc: self.rpc.clone(),
        }
        .close()
        .await
    }
}
impl Stream for BlobReader {
    type Item = Result<Vec<u8>, ClientError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.clean || self.failed {
            return Poll::Ready(None);
        }
        match self.source.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                if self.eof && self.bytes == self.descriptor.bytes.0 {
                    self.clean = true;
                    Poll::Ready(None)
                } else {
                    self.stop();
                    Poll::Ready(Some(Err(invalid())))
                }
            }
            Poll::Ready(Some(Ok(MediaStreamResponse::Blob(r))))
                if !self.eof
                    && r.sequence == self.next
                    && r.data.len() <= 65536
                    && (r.eof || !r.data.is_empty()) =>
            {
                self.next += 1;
                self.bytes = self.bytes.saturating_add(r.data.len() as u64);
                self.eof = r.eof;
                if self.bytes > self.descriptor.bytes.0
                    || (self.eof && self.bytes != self.descriptor.bytes.0)
                {
                    self.stop();
                    Poll::Ready(Some(Err(invalid())))
                } else {
                    Poll::Ready(Some(Ok(r.data)))
                }
            }
            Poll::Ready(Some(Err(e))) => {
                self.stop();
                Poll::Ready(Some(Err(e)))
            }
            _ => {
                self.stop();
                Poll::Ready(Some(Err(invalid())))
            }
        }
    }
}

pub struct DurableTextReader {
    rpc: MediaRpc,
    invocation: String,
    result: String,
    offset: u64,
    size: u64,
    pending: Vec<u8>,
    done: bool,
    poisoned: bool,
}
impl std::fmt::Debug for DurableTextReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableTextReader")
            .field("offset", &self.offset)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}
impl DurableTextReader {
    pub async fn next_chunk(&mut self) -> Result<Option<String>, ClientError> {
        if self.poisoned {
            return Err(invalid());
        }
        if self.done {
            return Ok(None);
        }
        self.poisoned = true;
        let r = (self.rpc)(MediaRequest::Text(wire::ReadTextResultRequest {
            invocation_id: self.invocation.clone(),
            result_id: self.result.clone(),
            offset: self.offset,
            max_bytes: 65536,
        }))
        .await?;
        let MediaResponse::Text(r) = r else {
            return Err(invalid());
        };
        if r.data.len() > 65536
            || r.next_offset != self.offset + r.data.len() as u64
            || r.next_offset > self.size
            || (r.data.is_empty() && !r.eof)
            || r.eof != (r.next_offset == self.size)
        {
            return Err(invalid());
        }
        self.pending.extend_from_slice(&r.data);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() && !r.eof => e.valid_up_to(),
            _ => return Err(invalid()),
        };
        let text = std::str::from_utf8(&self.pending[..valid])
            .map_err(|_| invalid())?
            .to_owned();
        self.pending.drain(..valid);
        if self.pending.len() > 3 || (r.eof && !self.pending.is_empty()) {
            return Err(invalid());
        }
        self.offset = r.next_offset;
        self.done = r.eof;
        self.poisoned = false;
        Ok(Some(text))
    }
}
