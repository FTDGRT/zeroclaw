//! Process-shared Channel conversation identity.
//!
//! The four Channel webhooks (WhatsApp, Linq, WATI, Nextcloud Talk) and the
//! Channel orchestrator resolve one opaque cross-turn conversation id per
//! routing/storage `conversation_history_key` through this shared state, so a
//! re-delivered or follow-up message reuses the same id instead of minting a
//! fresh UUID per inbound request. In durable mode the backend session record
//! is the single source of truth and the id is never mirrored here; in
//! memory-only mode the bounded LRU owns the id.

use std::num::NonZeroUsize;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::session_backend::SessionBackend;

/// Bound on the memory-only conversation-id LRU. Matches the orchestrator's
/// per-sender history bound so the id and history caches churn together.
pub const MAX_CHANNEL_SESSIONS: usize = 1000;

/// Shared Channel conversation identity for one daemon iteration.
///
/// One instance is constructed per reload iteration in the daemon and cloned
/// (`Arc`) into both the gateway and the channel orchestrator, so an inbound
/// webhook and the orchestrator's own turn mint site agree on the same id for
/// a given `conversation_history_key`.
pub struct ChannelSessionState {
    backend: Option<Arc<dyn SessionBackend>>,
    memory_conversation_ids: Mutex<lru::LruCache<String, String>>,
}

impl ChannelSessionState {
    /// Wrap an optional durable backend. `None` selects memory-only mode.
    pub fn new(backend: Option<Arc<dyn SessionBackend>>) -> Self {
        Self {
            backend,
            memory_conversation_ids: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(MAX_CHANNEL_SESSIONS)
                    .expect("channel session capacity is non-zero"),
            )),
        }
    }

    /// The durable backend, if any. The orchestrator reuses this handle for
    /// history persistence instead of opening a second backend owner.
    pub fn backend(&self) -> Option<&Arc<dyn SessionBackend>> {
        self.backend.as_ref()
    }

    /// Resolve the opaque cross-turn conversation id for a `history_key`.
    ///
    /// In durable mode the backend record is the single source of truth: the
    /// id is resolve-or-created there and NEVER mirrored into the memory LRU.
    /// In memory-only mode the bounded LRU owns the id; a fresh UUID is minted
    /// on first resolve and reused for the same key until rotation. The
    /// returned id is never the `history_key` or any routing/sender value. On
    /// backend failure the error is propagated verbatim; no fallback id is
    /// minted.
    pub fn resolve_conversation_id(&self, history_key: &str) -> std::io::Result<String> {
        if let Some(backend) = &self.backend {
            return backend.resolve_or_create_conversation_id(history_key);
        }

        let mut ids = self.memory_conversation_ids.lock();
        if let Some(id) = ids.get(history_key).cloned() {
            return Ok(id);
        }

        let id = uuid::Uuid::new_v4().to_string();
        ids.put(history_key.to_string(), id.clone());
        Ok(id)
    }

    /// Atomically clear the session history AND rotate the conversation id for
    /// a `history_key`, returning the fresh id.
    ///
    /// In durable mode this is one record-scoped backend op (clear history +
    /// new id together), never a caller-composed `delete + resolve`. In
    /// memory-only mode the LRU entry is overwritten with a fresh UUID. On
    /// backend failure the error is propagated verbatim.
    pub fn clear_and_rotate_conversation(&self, history_key: &str) -> std::io::Result<String> {
        if let Some(backend) = &self.backend {
            return backend.clear_and_rotate_conversation(history_key);
        }

        let id = uuid::Uuid::new_v4().to_string();
        self.memory_conversation_ids
            .lock()
            .put(history_key.to_string(), id.clone());
        Ok(id)
    }

    /// Number of ids held in the memory-only LRU. Test-only observable so the
    /// "durable mode never mirrors" invariant can be asserted; not production
    /// API.
    #[cfg(test)]
    fn memory_id_count_for_test(&self) -> usize {
        self.memory_conversation_ids.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use crate::session_sqlite::SqliteSessionBackend;
    use tempfile::TempDir;

    #[test]
    fn memory_only_reuses_one_uuid_per_history_key() {
        let state = ChannelSessionState::new(None);

        let first = state
            .resolve_conversation_id("whatsapp.main_room_alice")
            .unwrap();
        let second = state
            .resolve_conversation_id("whatsapp.main_room_alice")
            .unwrap();
        let other = state
            .resolve_conversation_id("whatsapp.main_room_bob")
            .unwrap();

        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(
            uuid::Uuid::parse_str(&first).unwrap().get_version(),
            Some(uuid::Version::Random)
        );
    }

    #[test]
    fn durable_state_reads_backend_without_mirroring_an_id() {
        let tmp = TempDir::new().unwrap();
        let backend: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
        let state = ChannelSessionState::new(Some(Arc::clone(&backend)));

        let first = state
            .resolve_conversation_id("linq.main_chat_alice")
            .unwrap();
        let second = state
            .resolve_conversation_id("linq.main_chat_alice")
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            backend
                .resolve_or_create_conversation_id("linq.main_chat_alice")
                .unwrap(),
            first
        );
        assert_eq!(state.memory_id_count_for_test(), 0);
    }

    #[test]
    fn memory_only_concurrent_first_resolve_materializes_one_entry() {
        // N threads resolve the same fresh key concurrently: the mutex around
        // the LRU serializes them so exactly ONE UUID wins and is reused, with
        // no duplicate entry materialized under contention.
        let state = Arc::new(ChannelSessionState::new(None));
        let key = Arc::new("whatsapp.main_room_alice".to_string());
        let n = 8;
        let barrier = Arc::new(std::sync::Barrier::new(n));
        let mut handles = vec![];
        for _ in 0..n {
            let state = Arc::clone(&state);
            let key = Arc::clone(&key);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                state.resolve_conversation_id(&key).unwrap()
            }));
        }
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = &ids[0];
        assert!(uuid::Uuid::parse_str(first).is_ok());
        for id in &ids {
            assert_eq!(id, first, "all concurrent resolves must converge on one id");
        }
        assert_eq!(
            state.memory_id_count_for_test(),
            1,
            "exactly one entry materialized under contention"
        );
    }
}
