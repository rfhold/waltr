use super::*;
use futures_util::task::{ArcWake, waker};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct WakeCount(AtomicUsize);
impl ArcWake for WakeCount {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn finish() -> wire::InteractRequest {
    wire::InteractRequest {
        frame: Some(wire::interact_request::Frame::Input(wire::InputEvent {
            event: Some(input_event::Event::Finish(wire::InputFinish {})),
        })),
    }
}

#[test]
fn native_input_eof_drains_preceding_frames_and_finish_without_terminal() {
    let queue = InteractionQueue::new();
    let preceding = wire::InteractRequest::default();
    queue.seed(preceding.clone());
    queue.seed(finish());
    queue.state.lock().unwrap().input_finished = true;
    let mut requests = InteractionRequests(queue.clone());
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    for expected in [preceding, finish()] {
        assert_eq!(
            Pin::new(&mut requests).poll_next(&mut cx),
            Poll::Ready(Some(expected))
        );
    }
    assert_eq!(
        Pin::new(&mut requests).poll_next(&mut cx),
        Poll::Ready(None)
    );
    drop(requests);
    let state = queue.state.lock().unwrap();
    assert_eq!(state.bytes, 0);
    assert!(!state.ended);
    assert!(!state.cancelled);
}

#[test]
fn native_input_finish_wakes_reader_after_finish_was_already_consumed() {
    let queue = InteractionQueue::new();
    queue.seed(finish());
    let mut requests = InteractionRequests(queue.clone());
    let count = Arc::new(WakeCount::default());
    let waker = waker(count.clone());
    let mut cx = Context::from_waker(&waker);
    assert_eq!(
        Pin::new(&mut requests).poll_next(&mut cx),
        Poll::Ready(Some(finish()))
    );
    assert!(Pin::new(&mut requests).poll_next(&mut cx).is_pending());
    queue.finish_input();
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        Pin::new(&mut requests).poll_next(&mut cx),
        Poll::Ready(None)
    );
}

#[test]
fn native_cancel_bypasses_queued_finish_and_emits_cancel_before_eof() {
    let queue = InteractionQueue::new();
    queue.seed(finish());
    queue.state.lock().unwrap().input_finished = true;
    queue.cancel();
    let mut requests = InteractionRequests(queue.clone());
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    let Poll::Ready(Some(request)) = Pin::new(&mut requests).poll_next(&mut cx) else {
        panic!("cancel must precede EOF");
    };
    assert!(matches!(
        request.frame,
        Some(wire::interact_request::Frame::Input(wire::InputEvent {
            event: Some(input_event::Event::Cancel(_))
        }))
    ));
    assert_eq!(
        Pin::new(&mut requests).poll_next(&mut cx),
        Poll::Ready(None)
    );
    assert_eq!(queue.state.lock().unwrap().bytes, 0);
    assert!(queue.take_native_cancel().is_none());
}

#[test]
fn native_post_eof_cancel_is_claimed_once_and_never_after_terminal() {
    let queue = InteractionQueue::new();
    queue.state.lock().unwrap().invocation_id = Some("exact-invocation".into());
    queue.cancel();
    assert!(queue.take_native_cancel().is_none());
    queue.state.lock().unwrap().native_closed = true;
    assert_eq!(
        queue.take_native_cancel().as_deref(),
        Some("exact-invocation")
    );
    assert!(queue.take_native_cancel().is_none());

    let ended = InteractionQueue::new();
    {
        let mut state = ended.state.lock().unwrap();
        state.invocation_id = Some("completed-invocation".into());
        state.native_closed = true;
    }
    ended.end();
    ended.cancel();
    assert!(ended.take_native_cancel().is_none());
}
