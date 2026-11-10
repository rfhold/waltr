use super::*;

pub(crate) struct Case {
    pub(crate) host: Host,
    service: Service,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
    id: u32,
    raw: Rc<Mutex<Raw>>,
    pub(crate) gate: Arc<tokio::sync::Semaphore>,
    credits: Arc<tokio::sync::Semaphore>,
}
impl Case {
    async fn new() -> Self {
        Self::from_fixture(fixture().await).await
    }
    pub(crate) async fn authenticated() -> Self {
        Self::from_fixture(fixture_with_bearer(true).await).await
    }
    async fn from_fixture(
        (host, service, stop, server): (
            Host,
            Service,
            CancellationToken,
            tokio::task::JoinHandle<()>,
        ),
    ) -> Self {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        service.state.lock().unwrap().checkout_gate = Some(gate.clone());
        let metadata = output_only(&host, true);
        let id = host
            .call(args("open", json!({"metadata":metadata})), None)
            .await
            .unwrap()
            .as_u64()
            .unwrap() as u32;
        wait(&host, "live").await;
        emit_blob(&service);
        let (raw, _) = host.raw_owner(id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if raw.lock().await.downloads.contains_key("saved") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            host,
            service,
            stop,
            server,
            id,
            raw,
            gate,
            credits: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    pub(crate) fn begin(&self) -> tokio::task::JoinHandle<Result<Value, String>> {
        let host = self.host.clone();
        let id = self.id;
        let permit = self.credits.clone().try_acquire_owned().unwrap();
        tokio::task::spawn_local(async move {
            host.call_inner(
                args("beginDownload", json!({"id":id,"port":"saved"})),
                None,
                Some(permit),
            )
            .await
        })
    }
    pub(crate) async fn entered(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while self.service.reader_opens.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            self.raw.try_lock().is_ok(),
            "checkout RPC must not hold raw state"
        );
        assert_eq!(self.credits.available_permits(), 0);
        assert_eq!(self.host.downloads.borrow().len(), 1);
    }
    async fn owner(&self) -> download::Owner {
        self.raw.lock().await.downloads["saved"]
            .owner
            .clone()
            .unwrap()
    }
    pub(crate) async fn closed_without_rescue(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while self.credits.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.service.reader_closes.load(Ordering::SeqCst), 1);
        self.host.join_downloads().await;
        assert!(self.host.downloads.borrow().is_empty());
    }
    pub(crate) async fn shutdown(self, abort_uncertain_fake: bool) {
        self.gate.add_permits(1);
        self.host.drain().await;
        if abort_uncertain_fake {
            // Only the synthetic failed-close fixture is aborted after asserting uncertainty.
            self.host.downloads.borrow_mut().abort_all();
        }
        self.host.join_downloads().await;
        self.stop.cancel();
        self.server.await.unwrap();
    }
    pub(crate) fn credentials(&self) -> NativeCredentialProvider {
        self.host.credentials.clone()
    }
    pub(crate) fn audio(&self) -> Arc<crate::native_audio::Host> {
        self.host.audio.clone()
    }
    pub(crate) fn authorized_closes(&self) -> Arc<AtomicUsize> {
        self.service.authorized_closes.clone()
    }
    pub(crate) fn fail_close(&self, fail: bool) {
        self.service.close_error.store(fail, Ordering::SeqCst);
    }
    pub(crate) fn tasks(&self) -> usize {
        self.host.downloads.borrow().len()
    }
    pub(crate) fn credits(&self) -> Arc<tokio::sync::Semaphore> {
        self.credits.clone()
    }
}

#[tokio::test]
async fn raw_checkout_cancel_drain_and_selection_retire_are_bounded_and_late_close_is_owned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for action in ["cancel", "drain", "selection"] {
                let case = Case::new().await;
                let pending = case.begin();
                case.entered().await;
                let owner = case.owner().await;
                let start = std::time::Instant::now();
                match action {
                    "cancel" => assert_eq!(
                        case.host
                            .call(args("cancel", json!({"id":case.id})), None)
                            .await
                            .unwrap(),
                        json!(false)
                    ),
                    "drain" => assert!(!case.host.drain().await),
                    _ => {
                        let mut snapshot = case.host.state.borrow().snapshot.clone();
                        snapshot.routines.definitions.definitions[0].routine_revision =
                            "replacement".into();
                        case.host.reconcile(&snapshot).await;
                        assert!(case.host.state.borrow().active.is_none());
                    }
                }
                assert!(start.elapsed() < std::time::Duration::from_secs(2));
                assert!(pending.await.unwrap().is_err());
                assert!(
                    !tokio::time::timeout(
                        std::time::Duration::from_millis(20),
                        owner.wait_closed()
                    )
                    .await
                    .unwrap(),
                    "cleanup deadline must not reset"
                );
                assert_eq!(case.service.reader_closes.load(Ordering::SeqCst), 0);
                assert_eq!(case.credits.available_permits(), 0);
                {
                    let mut audio = case.host.audio.lock();
                    audio.bind(2);
                    audio.access(true);
                }
                case.gate.add_permits(1);
                case.closed_without_rescue().await;
                assert!(case.host.audio.lock().allowed);
                assert_eq!(case.host.audio.lock().media_epoch, 0);
                case.shutdown(false).await;
            }
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_pinned_expiry_rejects_metadata_and_bytes_then_closes_late_reader() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let case = Case::new().await;
            let expiry = std::time::Instant::now() + std::time::Duration::from_millis(80);
            case.raw
                .lock()
                .await
                .downloads
                .get_mut("saved")
                .unwrap()
                .expires = expiry;
            let pending = case.begin();
            case.entered().await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), pending)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert!(std::time::Instant::now() >= expiry);
            assert!(
                case.host
                    .raw(
                        args("download", json!({"id":case.id,"port":"saved"})),
                        Vec::new()
                    )
                    .await
                    .is_err()
            );
            assert!(
                case.host
                    .call(
                        args("beginDownload", json!({"id":case.id,"port":"saved"})),
                        None
                    )
                    .await
                    .is_err()
            );
            assert_eq!(case.credits.available_permits(), 0);
            case.gate.add_permits(1);
            case.closed_without_rescue().await;
            case.shutdown(false).await;
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_dropped_waiter_retains_driver_handle_credit_and_late_close_authority() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let case = Case::new().await;
            let pending = case.begin();
            case.entered().await;
            let owner = case.owner().await;
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), owner.read(None))
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(case.host.downloads.borrow().len(), 1);
            assert_eq!(case.credits.available_permits(), 0);
            case.gate.add_permits(1);
            case.closed_without_rescue().await;
            case.shutdown(false).await;
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_dropped_ipc_reply_stops_waiter_without_losing_owner_or_credit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let case = Case::new().await;
            let (reply, result) = oneshot::channel();
            let request = super::super::Request {
                args: args("beginDownload", json!({"id":case.id,"port":"saved"})),
                selected: None,
                reply,
                ticket: (
                    Some(case.host.credentials.credential_revision()),
                    case.host.audio.lock().picker_epoch,
                ),
                _permit: case.credits.clone().try_acquire_owned().unwrap(),
            };
            let host = case.host.clone();
            let pending = tokio::task::spawn_local(async move {
                host.dispatch(request).await;
            });
            case.entered().await;
            let owner = case.owner().await;
            drop(result);
            tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), owner.read(None))
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(case.host.downloads.borrow().len(), 1);
            assert_eq!(case.credits.available_permits(), 0);
            case.gate.add_permits(1);
            case.closed_without_rescue().await;
            case.shutdown(false).await;
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_late_rpc_error_or_server_panic_never_becomes_clean() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for panic in [false, true] {
                let case = Case::new().await;
                case.service.checkout_error.store(!panic, Ordering::SeqCst);
                case.service.checkout_panic.store(panic, Ordering::SeqCst);
                let pending = case.begin();
                case.entered().await;
                let owner = case.owner().await;
                assert!(!case.host.drain().await);
                assert!(pending.await.unwrap().is_err());
                case.gate.add_permits(1);
                case.host.join_downloads().await;
                assert!(!owner.wait_closed().await);
                assert_eq!(case.service.reader_closes.load(Ordering::SeqCst), 0);
                assert_eq!(case.credits.available_permits(), 1);
                case.shutdown(false).await;
            }
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_uncertain_close_retains_actual_owner_and_never_reports_clean() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let case = Case::new().await;
            case.service.close_error.store(true, Ordering::SeqCst);
            let pending = case.begin();
            case.entered().await;
            let owner = case.owner().await;
            assert!(!case.host.drain().await);
            assert!(pending.await.unwrap().is_err());
            case.gate.add_permits(1);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while case.service.reader_closes.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!owner.wait_closed().await);
            assert_eq!(case.credits.available_permits(), 0);
            assert_eq!(case.host.downloads.borrow().len(), 1);
            assert!(case.host.downloads.borrow_mut().try_join_next().is_none());
            case.shutdown(true).await;
        })
        .await;
}

#[tokio::test]
async fn raw_checkout_control_renews_and_success_keeps_binary_chunks_bounded() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let case = Case::new().await;
            let pending = case.begin();
            case.entered().await;
            tokio::time::timeout(std::time::Duration::from_secs(7), async {
                while case.service.renewals.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            case.gate.add_permits(1);
            let descriptor = pending.await.unwrap().unwrap();
            assert!(!descriptor.to_string().contains("SECRET"));
            case.host
                .call(args("finishInput", json!({"id":case.id})), None)
                .await
                .unwrap();
            wait(&case.host, "succeeded").await;
            for expected in [65536, 34465, 0] {
                let bytes = case
                    .host
                    .raw(
                        args("download", json!({"id":case.id,"port":"saved"})),
                        Vec::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(bytes.len(), expected);
            }
            case.host
                .call(
                    args("closeDownload", json!({"id":case.id,"port":"saved"})),
                    None,
                )
                .await
                .unwrap();
            case.closed_without_rescue().await;
            case.shutdown(false).await;
        })
        .await;
}
