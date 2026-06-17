//! Agent timeline stream types.

use futures_util::Stream;

use crate::{AgentTimelineEvent, Result};

/// Boxed stream of cursor-assigned agent timeline events.
pub type BoxAgentTimelineStream<'a> =
    std::pin::Pin<Box<dyn Stream<Item = Result<AgentTimelineEvent>> + Send + 'a>>;
