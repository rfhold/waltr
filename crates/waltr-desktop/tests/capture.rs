//! Synthetic source terminal classification; no hardware access.
use waltr_audio_io::{Config, Error as NativeError, Terminal};
use waltr_desktop::capture::{CaptureWorker, Error, Factory, Source};
use std::sync::Arc;

struct Failed(NativeError);
impl Factory for Failed {
    fn open(&self, _: Config) -> Result<Box<dyn Source>, Error> {
        Ok(Box::new(Failed(self.0.clone())))
    }
}
impl Source for Failed {
    fn frame(&mut self) -> Option<Vec<u8>> {
        None
    }
    fn terminal(&self) -> Terminal {
        Terminal::Failed(self.0.clone())
    }
    fn stop(&self) {}
    fn shutdown(self: Box<Self>) {}
}
#[tokio::test]
async fn native_terminal_categories_survive_failure_lane_and_join() {
    for (native, expected) in [
        (NativeError::CallbackOverflow, Error::CallbackOverflow),
        (
            NativeError::CallbackPoolExhausted,
            Error::CallbackPoolExhausted,
        ),
        (NativeError::CallbackQueueFull, Error::CallbackQueueFull),
        (NativeError::NonFiniteSample, Error::Conversion),
        (NativeError::MisalignedInput, Error::Conversion),
        (NativeError::Limit, Error::ResourceLimit),
        (NativeError::WorkerPanicked, Error::Worker),
        (
            NativeError::PreacceptBudgetExceeded,
            Error::PreacceptBudgetExceeded,
        ),
        (
            NativeError::Backend("SECRET_DEVICE_ERROR".into()),
            Error::Backend,
        ),
    ] {
        let mut worker = CaptureWorker::start(Arc::new(Failed(native)), Config::default());
        assert!(worker.frames.recv().await.is_none());
        assert_eq!(worker.check_failure(), Err(expected));
        assert_eq!(worker.join().await, Err(expected));
        assert_eq!(worker.join().await, Err(expected));
        assert!(!format!("{expected:?}").contains("SECRET"));
    }
}
struct Panics;
impl Factory for Panics {
    fn open(&self, _: Config) -> Result<Box<dyn Source>, Error> {
        panic!("synthetic worker panic")
    }
}
#[tokio::test]
async fn worker_panic_is_not_backend() {
    let mut worker = CaptureWorker::start(Arc::new(Panics), Config::default());
    assert_eq!(worker.join().await, Err(Error::Worker));
}
