//! Exact, host-owned completed-context identities.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{SessionCopy, SessionCopyError, SessionId, SessionRecord};
use crate::TimelineCursor;

/// Coherent effective completed capture and exact source identity from one loaded record.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContextCapture {
    /// Exact source identity, not a guest authorization grant.
    pub identity: ContextIdentity,
    /// Immutable mask-effective context and captured inference/definition.
    pub completed: super::CompletedSessionCapture,
}

/// Exact context identity; possession does not authorize access to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextIdentity {
    digest: [u8; 32],
    source_digest: [u8; 32],
    revision: super::Revision,
    mask_commits: u64,
    boundary: TimelineCursor,
    authority: String,
    source_id: SessionId,
    incarnation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_original_digest: Option<[u8; 32]>,
}

impl ContextIdentity {
    /// Pin completed context to a host-validated authority identity, never a guest selector.
    ///
    /// # Errors
    /// Rejects legacy records without an incarnation, invalid boundaries, and oversized identities.
    pub fn capture(
        record: &SessionRecord,
        boundary: TimelineCursor,
        authority: &str,
    ) -> Result<Self, SessionCopyError> {
        let incarnation = record
            .metadata
            .incarnation
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SessionCopyError::InvalidSource("context requires a durable incarnation".into())
            })?;
        if authority.is_empty() || authority.len() > 128 {
            return Err(SessionCopyError::InvalidSource(
                "context requires a durable incarnation and bounded host authority".into(),
            ));
        }
        if record.journal.len() > 100_000 {
            return Err(SessionCopyError::InvalidSource(
                "context journal exceeds limit".into(),
            ));
        }
        SessionCopy::from_record(record, boundary)?;
        let digest = Self::digest(record, boundary, authority)?;
        Ok(Self {
            digest,
            source_digest: Self::source_digest(record, boundary, authority)?,
            revision: record.revision,
            mask_commits: mask_commits(record),
            boundary,
            authority: authority.to_owned(),
            source_id: record.id.clone(),
            incarnation: incarnation.to_owned(),
            completed_original_digest: Some(Self::completed_original_digest(
                record, boundary, authority,
            )?),
        })
    }

    /// Exact completed boundary. Hosts encode this losslessly as a string for guests.
    #[must_use]
    pub const fn boundary(&self) -> TimelineCursor {
        self.boundary
    }

    /// Opaque observation provenance, not access or mutation authority.
    pub fn provenance(&self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Compare against current durable state, including edits after the completed boundary.
    #[must_use]
    pub fn matches(&self, record: &SessionRecord) -> bool {
        Self::digest(record, self.boundary, &self.authority)
            .is_ok_and(|digest| digest == self.digest)
    }

    /// Compare the pinned source while accounting only for committed mask transactions.
    /// Callers must additionally compare the complete expected active-mask map atomically.
    #[must_use]
    pub fn matches_completed_source(&self, record: &SessionRecord) -> bool {
        if record.journal.len() > 100_000 {
            return false;
        }
        let Some(additional_masks) = mask_commits(record).checked_sub(self.mask_commits) else {
            return false;
        };
        self.revision.get().checked_add(additional_masks) == Some(record.revision.get())
            && Self::source_digest(record, self.boundary, &self.authority)
                .is_ok_and(|digest| digest == self.source_digest)
    }

    /// Compare source incarnation and host-authorized context scope, without claiming coverage.
    pub fn same_scope(&self, other: &Self) -> bool {
        self.source_id == other.source_id
            && self.incarnation == other.incarnation
            && self.authority == other.authority
    }

    /// Compare host-captured original projections, without granting access or mutation authority.
    #[must_use]
    pub fn same_completed_original(&self, other: &Self) -> bool {
        self.same_scope(other)
            && self.boundary == other.boundary
            && self.completed_original_digest.is_some()
            && self.completed_original_digest == other.completed_original_digest
    }

    /// Validate only the original projection at this boundary, not current mutation authority.
    #[must_use]
    pub fn completed_original_matches(&self, record: &SessionRecord) -> bool {
        self.completed_original_digest.is_some_and(|expected| {
            Self::completed_original_digest(record, self.boundary, &self.authority)
                .is_ok_and(|actual| actual == expected)
        })
    }

    fn completed_original_digest(
        record: &SessionRecord,
        boundary: TimelineCursor,
        authority: &str,
    ) -> Result<[u8; 32], SessionCopyError> {
        let copy = SessionCopy::from_record(record, boundary)?;
        let mut writer = IdentityWriter {
            hash: Sha256::new(),
            bytes: 0,
        };
        let rollbacks = record
            .journal
            .iter()
            .enumerate()
            .filter(|(_, event)| matches!(event, super::SessionJournalEvent::Rollback { .. }))
            .collect::<Vec<_>>();
        serde_json::to_writer(
            &mut writer,
            &(
                "waltr.completed-original/v1",
                &record.id,
                &record.metadata.incarnation,
                authority,
                boundary,
                &record.agent_definition,
                &record.inference_settings,
                copy.materialize(&record.id),
                rollbacks,
            ),
        )
        .map_err(|_| {
            SessionCopyError::InvalidSource("completed original exceeds encoding limit".into())
        })?;
        Ok(writer.hash.finalize().into())
    }

    fn digest(
        record: &SessionRecord,
        boundary: TimelineCursor,
        authority: &str,
    ) -> Result<[u8; 32], SessionCopyError> {
        if record.journal.len() > 100_000 {
            return Err(SessionCopyError::InvalidSource(
                "context journal exceeds limit".into(),
            ));
        }
        let mut writer = IdentityWriter {
            hash: Sha256::new(),
            bytes: 0,
        };
        serde_json::to_writer(
            &mut writer,
            &(
                &record.id,
                &record.metadata.incarnation,
                &record.metadata.values,
                &record.metadata.item_values,
                record.revision,
                boundary,
                &record.inference_settings,
                &record.agent_definition,
                record.timeline_cursor_high_water(),
                record.effective_journal_events(),
                authority,
            ),
        )
        .map_err(|_| {
            SessionCopyError::InvalidSource("context identity exceeds encoding limit".into())
        })?;
        Ok(writer.hash.finalize().into())
    }

    fn source_digest(
        record: &SessionRecord,
        boundary: TimelineCursor,
        authority: &str,
    ) -> Result<[u8; 32], SessionCopyError> {
        if record.journal.len() > 100_000 {
            return Err(SessionCopyError::InvalidSource(
                "context journal exceeds limit".into(),
            ));
        }
        let source = record
            .journal
            .iter()
            .filter(|event| !matches!(event, super::SessionJournalEvent::MasksMutated { .. }))
            .collect::<Vec<_>>();
        let mut writer = IdentityWriter {
            hash: Sha256::new(),
            bytes: 0,
        };
        serde_json::to_writer(
            &mut writer,
            &(
                &record.id,
                &record.metadata.incarnation,
                boundary,
                &record.inference_settings,
                &record.agent_definition,
                source,
                authority,
            ),
        )
        .map_err(|_| {
            SessionCopyError::InvalidSource("source identity exceeds encoding limit".into())
        })?;
        Ok(writer.hash.finalize().into())
    }
}

fn mask_commits(record: &SessionRecord) -> u64 {
    record
        .journal
        .iter()
        .filter(|event| matches!(event, super::SessionJournalEvent::MasksMutated { .. }))
        .count() as u64
}

pub(super) fn json_within_limit(value: &impl Serialize, limit: usize) -> bool {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(std::io::Error::other("JSON byte limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { bytes: 0, limit }, value).is_ok()
}

struct IdentityWriter {
    hash: Sha256,
    bytes: usize,
}
impl std::io::Write for IdentityWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > 16 * 1024 * 1024 {
            return Err(std::io::Error::other("context identity limit"));
        }
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
