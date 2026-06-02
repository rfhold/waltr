//! Stream reducer responsibilities.

use crate::{ClientState, StreamEvent};

pub fn reduce(state: &mut ClientState, event: StreamEvent) {
    match event {
        StreamEvent::Chunk { stream_id, text } => {
            state.stream_mut(stream_id).append(&text);
        }
        StreamEvent::Finished { stream_id } => state.stream_mut(stream_id).finish(),
        StreamEvent::Error { stream_id, message } => {
            state.stream_mut(stream_id).fail(message);
        }
    }
}
