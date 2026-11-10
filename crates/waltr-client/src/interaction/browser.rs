//! Testable browser ownership and bounded binary checkout state.
use super::invalid;
use crate::ClientError;
use std::{cell::Cell, collections::VecDeque, rc::Rc};

pub(crate) struct PendingAdmission {
    slot: Rc<Cell<Option<u32>>>,
    id: u32,
}
impl PendingAdmission {
    pub(crate) fn reserve(slot: Rc<Cell<Option<u32>>>, id: u32) -> Result<Self, ClientError> {
        if slot.get().is_some() {
            return Err(invalid());
        }
        slot.set(Some(id));
        Ok(Self { slot, id })
    }
    pub(crate) fn current(&self) -> bool {
        self.slot.get() == Some(self.id)
    }
}
impl Drop for PendingAdmission {
    fn drop(&mut self) {
        if self.current() {
            self.slot.set(None);
        }
    }
}

#[derive(Default)]
pub(crate) struct OutputLifecycle {
    drain_until: Option<i64>,
}
impl OutputLifecycle {
    pub(crate) fn terminal(&mut self, now: i64, absolute: i64) {
        self.drain_until = Some(now.saturating_add(30000).min(absolute));
    }
    pub(crate) fn draining(&self) -> bool {
        self.drain_until.is_some()
    }
    pub(crate) fn expired(&self, now: i64, deadlines: (i64, i64, i64), accepted: bool) -> bool {
        let (attach, lease, absolute) = deadlines;
        match self.drain_until {
            Some(until) => now >= until,
            None => now >= lease || now >= absolute || (!accepted && now >= attach),
        }
    }
}
struct OutputEntry {
    id: u32,
    metadata: Option<serde_json::Value>,
    bytes: Option<Vec<u8>>,
}
#[derive(Default)]
pub(crate) struct MediaOutputBuffer {
    entries: VecDeque<OutputEntry>,
    bytes: usize,
    next: u32,
}
impl MediaOutputBuffer {
    pub(crate) fn push(&mut self, metadata: serde_json::Value) -> Result<(), ClientError> {
        self.insert(metadata, None)
    }
    pub(crate) fn audio(
        &mut self,
        stream: String,
        sequence: u64,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        if data.is_empty() || data.len() > 65536 {
            return Err(invalid());
        }
        self.insert(serde_json::json!({"type":"audio","streamId":stream,"sequence":sequence.to_string(),"byteLength":data.len()}),Some(data))
    }
    fn insert(
        &mut self,
        mut metadata: serde_json::Value,
        bytes: Option<Vec<u8>>,
    ) -> Result<(), ClientError> {
        let total = self
            .bytes
            .checked_add(bytes.as_ref().map_or(0, Vec::len))
            .ok_or_else(invalid)?;
        if self.entries.len() >= 32 || total > 256 * 1024 {
            return Err(invalid());
        }
        let id = self.next.checked_add(1).ok_or_else(invalid)?;
        if bytes.is_some() {
            metadata["chunkId"] = serde_json::json!(id);
        }
        self.next = id;
        self.bytes = total;
        self.entries.push_back(OutputEntry {
            id,
            metadata: Some(metadata),
            bytes,
        });
        Ok(())
    }
    pub(crate) fn metadata(&mut self) -> Option<serde_json::Value> {
        let index = self.entries.iter().position(|e| e.metadata.is_some())?;
        let value = self.entries[index].metadata.take();
        if self.entries[index].bytes.is_none() {
            self.entries.remove(index);
        }
        value
    }
    pub(crate) fn take_bytes(&mut self, id: u32) -> Result<Vec<u8>, ClientError> {
        let index = self
            .entries
            .iter()
            .position(|e| e.id == id && e.metadata.is_none() && e.bytes.is_some())
            .ok_or_else(invalid)?;
        let data = self.entries.remove(index).unwrap().bytes.unwrap();
        self.bytes -= data.len();
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn blocked_producer_has_one_credit_and_busy_calls_do_not_copy_payloads() {
        use futures_util::{FutureExt, lock::Mutex};
        let lane = Rc::new(Mutex::new(()));
        let copies = Cell::new(0usize);
        let mut blocked = Box::pin(async {
            let _credit = lane.try_lock().unwrap();
            copies.set(copies.get() + 1);
            futures_util::future::pending::<()>().await;
        });
        assert!(blocked.as_mut().now_or_never().is_none());
        for _ in 0..10000 {
            if let Some(_credit) = lane.try_lock() {
                copies.set(copies.get() + 1);
            }
        }
        assert_eq!(copies.get(), 1);
        drop(blocked);
        assert!(lane.try_lock().is_some());
        let newer = Mutex::new(());
        let _new = newer.try_lock().unwrap();
        assert!(lane.try_lock().is_some());
    }
    #[tokio::test]
    async fn dropped_admission_future_permits_an_explicit_new_start_without_retry() {
        use futures_util::FutureExt;
        let slot = Rc::new(Cell::new(None));
        let mut pending = Box::pin(async {
            let _guard = PendingAdmission::reserve(slot.clone(), 1).unwrap();
            futures_util::future::pending::<()>().await;
        });
        assert!(pending.as_mut().now_or_never().is_none());
        drop(pending);
        let next = PendingAdmission::reserve(slot.clone(), 2).unwrap();
        assert!(next.current());
    }
    #[test]
    fn pending_failure_rejection_and_drop_release_only_the_matching_identity() {
        let slot = Rc::new(Cell::new(None));
        for (id, reason) in [(1, "rejected"), (2, "lost-ack"), (3, "stale-admission")] {
            let result: Result<(), &str> = {
                let guard = PendingAdmission::reserve(slot.clone(), id).unwrap();
                assert!(guard.current());
                Err(reason)
            };
            assert_eq!(result, Err(reason));
            assert_eq!(slot.get(), None);
        }
        let old = PendingAdmission::reserve(slot.clone(), 4).unwrap();
        slot.set(None);
        let new = PendingAdmission::reserve(slot.clone(), 5).unwrap();
        assert!(!old.current());
        drop(old);
        assert!(new.current());
        drop(new);
        assert_eq!(slot.get(), None);
    }
    #[test]
    fn terminal_drain_ignores_lease_ticks_but_requires_bounded_eof() {
        let mut state = OutputLifecycle::default();
        assert!(state.expired(20, (5, 10, 100000), true));
        state.terminal(9, 100000);
        assert!(!state.expired(20, (5, 10, 100000), true));
        assert!(state.draining());
        assert!(state.expired(30009, (5, 10, 100000), true));
    }
    #[test]
    fn pcm_checkout_is_typed_single_use_and_bounded_after_metadata_consumption() {
        let mut buffer = MediaOutputBuffer::default();
        let bytes = vec![17; 65536];
        for n in 0..4 {
            buffer.audio("pcm".into(), n, bytes.clone()).unwrap();
        }
        assert!(buffer.audio("pcm".into(), 4, vec![0; 2]).is_err());
        let meta = buffer.metadata().unwrap();
        assert!(meta.get("data").is_none());
        assert!(!meta.to_string().contains("[17"));
        let id = meta["chunkId"].as_u64().unwrap() as u32;
        assert_eq!(buffer.take_bytes(id).unwrap(), bytes);
        assert!(buffer.take_bytes(id).is_err());
        buffer.audio("pcm".into(), 4, vec![0; 2]).unwrap();
        while buffer.metadata().is_some() {}
        assert_eq!(buffer.entries.len(), 4);
        let mut newer = MediaOutputBuffer::default();
        assert!(newer.take_bytes(id).is_err());
    }
    #[test]
    fn abandoned_metadata_consumers_cannot_evade_event_budget() {
        let mut buffer = MediaOutputBuffer::default();
        for n in 0..32 {
            buffer.audio("pcm".into(), n, vec![0; 2]).unwrap();
            buffer.metadata().unwrap();
        }
        assert!(buffer.push(serde_json::json!({"type":"close"})).is_err());
        assert_eq!(buffer.bytes, 64);
    }
}
