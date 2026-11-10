//! Durable execution identity and bounded committed-state receipts.

#![allow(missing_docs)]

use serde::{Deserialize, Serialize};

use super::storage::{SessionStoreError, SessionStoreResult};
use super::types::SessionMetadata;
use super::{InferenceSettings, Revision, SessionId, SessionRecord};
use crate::timeline::{AgentTimelineEvent, TimelineCursor};

/// Execution freshness, independent of the public record revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExecutionSequence(u64);

impl ExecutionSequence {
    pub const ZERO: Self = Self(0);
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    pub(super) const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Freshness is bound to the durable incarnation, never the process-local list identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionToken {
    pub session_id: SessionId,
    pub incarnation: String,
    pub sequence: ExecutionSequence,
}

/// Coherent non-journal facts observed inside the committing transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCommitReceipt {
    pub revision: Revision,
    pub execution_token: ExecutionToken,
    pub metadata: SessionMetadata,
    pub inference_settings: Option<InferenceSettings>,
    pub agent_definition: Option<serde_json::Value>,
    pub timeline_cursor_high_water: TimelineCursor,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineAppendReceipt {
    pub commit: SessionCommitReceipt,
    pub events: Vec<AgentTimelineEvent>,
}

impl SessionRecord {
    pub fn execution_token(&self) -> SessionStoreResult<ExecutionToken> {
        let incarnation = self.metadata.incarnation.clone().ok_or_else(|| {
            SessionStoreError::ExecutionCompatibility {
                id: self.id.clone(),
            }
        })?;
        Ok(ExecutionToken {
            session_id: self.id.clone(),
            incarnation,
            sequence: self.execution_sequence,
        })
    }

    pub(super) fn check_execution_token(
        &self,
        expected: &ExecutionToken,
    ) -> SessionStoreResult<()> {
        let actual = self.execution_token()?;
        if &actual != expected {
            return Err(SessionStoreError::StaleExecution {
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub(super) fn advance_revision(&mut self, execution: bool) {
        self.revision = self.revision.next();
        if execution {
            self.execution_sequence = self.execution_sequence.next();
        }
    }

    pub(crate) fn commit_receipt(&self) -> SessionStoreResult<SessionCommitReceipt> {
        Ok(SessionCommitReceipt {
            revision: self.revision,
            execution_token: self.execution_token()?,
            metadata: self.metadata.clone(),
            inference_settings: self.inference_settings.clone(),
            agent_definition: self.agent_definition.clone(),
            timeline_cursor_high_water: self.timeline_cursor_high_water(),
        })
    }

    pub(crate) fn adopt_commit(&mut self, receipt: SessionCommitReceipt) {
        assert_eq!(self.id, receipt.execution_token.session_id);
        assert_eq!(
            self.metadata.incarnation.as_deref(),
            Some(receipt.execution_token.incarnation.as_str())
        );
        self.revision = receipt.revision;
        self.execution_sequence = receipt.execution_token.sequence;
        self.metadata = receipt.metadata;
        self.inference_settings = receipt.inference_settings;
        self.agent_definition = receipt.agent_definition;
        self.timeline_cursor_high_water = receipt.timeline_cursor_high_water;
    }
}
