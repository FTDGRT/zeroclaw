//! Process-shared Channel conversation identity, history, and turn lifecycle.
//!
//! The four Channel webhooks (WhatsApp, Linq, WATI, Nextcloud Talk) and the
//! Channel orchestrator resolve one opaque cross-turn conversation id per
//! routing/storage `conversation_history_key` through this shared state, so a
//! re-delivered or follow-up message reuses the same id instead of minting a
//! fresh UUID per inbound request. In durable mode the backend session record
//! is the single source of truth and the id is never mirrored into the cache;
//! in memory-only mode the bounded LRU owns both the id and the history as one
//! record.
//!
//! The captured conversation id is also a write fence: history append / update
//! / rollback / compaction are conditional on the record still carrying the id
//! the turn captured before any async work began. A stale (rotated) or deleted
//! result is an expected lifecycle race, not retried, and must not recreate a
//! record. Active Channel turn workers register a lease here so `/new`,
//! `/clear`, and delete can cancel and wait for competing workers (excluding
//! the commanding turn) before mutating; the conditional write remains the
//! final correctness boundary.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::{Mutex as TokioMutex, Notify};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::model_provider::ChatMessage;

use crate::session_backend::{ConditionalSessionWrite, SessionBackend};

/// Bound on the memory-only session LRU. Matches the orchestrator's
/// per-sender history bound so the record (history + id together) churns at
/// the same rate as before.
pub const MAX_CHANNEL_SESSIONS: usize = 1000;

/// In-memory history + conversation id for a memory-only session. The two are
/// ONE LRU record so they evict together: a history eviction never strands a
/// stale id, and an id rotation never orphans old history.
#[derive(Debug, Clone)]
struct MemorySessionRecord {
    history: Vec<ChatMessage>,
    conversation_id: String,
}

/// Cached view of one session. `Memory` owns both history and id (memory-only
/// mode). `DurableHistory` is a bounded materialized view of backend history
/// only - the id is never mirrored here, it is resolved from the backend each
/// time. The enum keeps the two modes from sharing storage.
enum CachedChannelSession {
    Memory(MemorySessionRecord),
    DurableHistory(Vec<ChatMessage>),
}

/// Completion signal for one in-flight turn. A reset/delete that has copied a
/// turn's `Arc<TurnCompletion>` awaits [`TurnCompletion::wait`]; the worker or
/// lease drop signals [`TurnCompletion::mark_done`]. Wait registers for
/// notification before checking the flag, closing the lost-wakeup window while
/// still allowing a late waiter to return immediately.
struct TurnCompletion {
    done: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl TurnCompletion {
    fn new() -> Self {
        Self {
            done: std::sync::atomic::AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        let notified = self.notify.notified();
        if self.is_done() {
            return;
        }
        notified.await;
    }
}

/// Registered state for one in-flight turn under a given history key.
#[derive(Clone)]
struct ActiveTurnState {
    cancellation: CancellationToken,
    completion: Arc<TurnCompletion>,
}

/// `history_key -> turn_id -> state`. A turn is registered under the key it
/// captured so reset/delete can find and cancel competing workers for that key
/// only (independent keys never interfere).
type ActiveTurnMap = HashMap<String, HashMap<u64, ActiveTurnState>>;

/// Handle returned by [`ChannelSessionState::register_turn`]. The worker keeps
/// it for the turn's lifetime and calls [`ChannelSessionState::complete_turn`]
/// on normal return paths for prompt map cleanup. Synchronous `Drop` marks the
/// completion as a panic/abort safety net; registry operations prune that stale
/// completed entry later. The commanding turn passes its `id` to
/// `reset_session`/`delete_session` so the lifecycle op cancels and waits for
/// the OTHER workers but never itself (which would deadlock).
pub struct ChannelTurnLease {
    key: String,
    id: u64,
    cancellation: CancellationToken,
    completion: Arc<TurnCompletion>,
}

impl ChannelTurnLease {
    /// Monotonic id of this turn within the active map.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Cancellation token for this turn. Plumbed into the turn body so a
    /// competing reset/delete can cancel it mid-flight.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Drop for ChannelTurnLease {
    fn drop(&mut self) {
        self.completion.mark_done();
    }
}

/// Shared Channel conversation identity, history, and turn lifecycle for one
/// daemon iteration.
///
/// One instance is constructed per reload iteration in the daemon and cloned
/// (`Arc`) into both the gateway and the channel orchestrator, so an inbound
/// webhook and the orchestrator's own turn mint site agree on the same id for
/// a given `conversation_history_key`.
pub struct ChannelSessionState {
    backend: Option<Arc<dyn SessionBackend>>,
    cache: Mutex<lru::LruCache<String, CachedChannelSession>>,
    active_turns: TokioMutex<ActiveTurnMap>,
    next_turn_id: AtomicU64,
}

impl ChannelSessionState {
    /// Wrap an optional durable backend. `None` selects memory-only mode.
    pub fn new(backend: Option<Arc<dyn SessionBackend>>) -> Self {
        Self {
            backend,
            cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(MAX_CHANNEL_SESSIONS)
                    .expect("channel session capacity is non-zero"),
            )),
            active_turns: TokioMutex::new(HashMap::new()),
            next_turn_id: AtomicU64::new(0),
        }
    }

    /// Test-only constructor with an explicit LRU capacity so the eviction
    /// invariant (history + id evict together) can be exercised without
    /// inserting 1000 records.
    #[cfg(test)]
    fn with_capacity(backend: Option<Arc<dyn SessionBackend>>, capacity: usize) -> Self {
        Self {
            backend,
            cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(capacity).expect("test capacity is non-zero"),
            )),
            active_turns: TokioMutex::new(HashMap::new()),
            next_turn_id: AtomicU64::new(0),
        }
    }

    /// The durable backend, if any. The orchestrator reuses this handle for
    /// history persistence instead of opening a second backend owner.
    pub fn backend(&self) -> Option<&Arc<dyn SessionBackend>> {
        self.backend.as_ref()
    }

    // ── conversation-id resolve / rotate ───────────────────────────────

    /// Resolve the opaque cross-turn conversation id for a `history_key`.
    ///
    /// In durable mode the backend record is the single source of truth: the
    /// id is resolve-or-created there and NEVER mirrored into the cache. In
    /// memory-only mode the LRU owns a `Memory` record (history + id); a fresh
    /// UUID is minted on first resolve and reused until rotation. On backend
    /// failure the error is propagated verbatim; no fallback id is minted.
    pub fn resolve_conversation_id(&self, history_key: &str) -> std::io::Result<String> {
        if let Some(backend) = &self.backend {
            return backend.resolve_or_create_conversation_id(history_key);
        }

        let mut cache = self.cache.lock();
        if let Some(CachedChannelSession::Memory(rec)) = cache.get(history_key) {
            return Ok(rec.conversation_id.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        cache.put(
            history_key.to_string(),
            CachedChannelSession::Memory(MemorySessionRecord {
                history: Vec::new(),
                conversation_id: id.clone(),
            }),
        );
        Ok(id)
    }

    /// Atomically clear the session history AND rotate the conversation id for
    /// a `history_key`, returning the fresh id.
    ///
    /// In durable mode this is one record-scoped backend op (clear history +
    /// new id together) and also drops any `DurableHistory` cache view so the
    /// next load re-reads the cleared backend. In memory-only mode the
    /// `Memory` record's id is replaced and its history cleared (the entry is
    /// kept, not evicted). On backend failure the error is propagated verbatim.
    pub fn clear_and_rotate_conversation(&self, history_key: &str) -> std::io::Result<String> {
        if let Some(backend) = &self.backend {
            let id = backend.clear_and_rotate_conversation(history_key)?;
            self.cache.lock().pop(history_key);
            return Ok(id);
        }

        let mut cache = self.cache.lock();
        let id = uuid::Uuid::new_v4().to_string();
        match cache.get_mut(history_key) {
            Some(CachedChannelSession::Memory(rec)) => {
                rec.conversation_id = id.clone();
                rec.history.clear();
            }
            _ => {
                cache.put(
                    history_key.to_string(),
                    CachedChannelSession::Memory(MemorySessionRecord {
                        history: Vec::new(),
                        conversation_id: id.clone(),
                    }),
                );
            }
        }
        Ok(id)
    }

    // ── history read / conditional mutations ───────────────────────────

    /// Load the current history view for a key.
    ///
    /// Durable: return the cached `DurableHistory` view if present, else load
    /// from the backend and cache it (bounded). Memory-only: return the
    /// `Memory` record's history (empty if the record is absent).
    pub fn load_history(&self, key: &str) -> Vec<ChatMessage> {
        if let Some(backend) = &self.backend {
            let mut cache = self.cache.lock();
            if let Some(CachedChannelSession::DurableHistory(history)) = cache.get(key) {
                return history.clone();
            }
            // Keep backend materialization and cache installation atomic with
            // lifecycle invalidation. Reset/delete release their backend lock
            // before taking this cache lock, so this order cannot form a cycle.
            let messages = backend.load(key);
            cache.put(
                key.to_string(),
                CachedChannelSession::DurableHistory(messages.clone()),
            );
            messages
        } else {
            let mut cache = self.cache.lock();
            match cache.get(key) {
                Some(CachedChannelSession::Memory(rec)) => rec.history.clone(),
                _ => Vec::new(),
            }
        }
    }

    /// Prime the durable history cache view for `key` with `messages`.
    ///
    /// Used by startup rehydration to install a repaired / pruned view (orphan
    /// user-turn closure markers added, orphaned tool messages removed) so the
    /// first turn does not re-read the un-pruned backend history. No-op in
    /// memory-only mode (rehydration only runs when a backend is present). The
    /// conversation id is NOT mirrored - this only installs a `DurableHistory`
    /// view; the id is still resolved from the backend on demand.
    pub fn prime_durable_history(&self, key: &str, messages: Vec<ChatMessage>) {
        if self.backend.is_some() {
            let mut cache = self.cache.lock();
            cache.put(
                key.to_string(),
                CachedChannelSession::DurableHistory(messages),
            );
        }
    }

    /// Append `message` iff the record still carries `expected_id`. Durable
    /// delegates to the backend conditional method and only updates the
    /// `DurableHistory` cache view on `Applied`; stale/deleted leaves the cache
    /// untouched. Memory-only compares the id under one cache lock and mutates
    /// in place. `max_history` bounds the retained tail.
    pub fn append_history_if_current(
        &self,
        key: &str,
        expected_id: &str,
        message: ChatMessage,
        max_history: usize,
    ) -> std::io::Result<ConditionalSessionWrite> {
        if let Some(backend) = &self.backend {
            let status = backend.append_if_conversation_matches(key, expected_id, &message)?;
            if status == ConditionalSessionWrite::Applied {
                let mut cache = self.cache.lock();
                if let Some(CachedChannelSession::DurableHistory(history)) = cache.get_mut(key) {
                    push_bounded(history, message, max_history);
                }
            }
            return Ok(status);
        }

        let mut cache = self.cache.lock();
        match cache.get_mut(key) {
            Some(CachedChannelSession::Memory(rec)) if rec.conversation_id == expected_id => {
                push_bounded(&mut rec.history, message, max_history);
                Ok(ConditionalSessionWrite::Applied)
            }
            Some(CachedChannelSession::Memory(_)) => Ok(ConditionalSessionWrite::Stale),
            _ => Ok(ConditionalSessionWrite::Deleted),
        }
    }

    /// Remove the last message iff the record still carries `expected_id` and
    /// (memory-only) the last message matches `expected_role`/`expected_content`.
    ///
    /// Durable delegates to the backend conditional `remove_last`; the caller
    /// is responsible for only rolling back a user turn it just appended (and
    /// whose append returned `Applied`), so the orphan is the last row. Turn
    /// serialization (cancel+wait on reset/delete) keeps it the last row; the
    /// id fence is the final boundary for cross-process races. Memory-only does
    /// the content-checked pop atomically under the cache lock. A matching
    /// record whose last message does not match is a no-op `Applied`. The
    /// `Memory` record is kept (NOT evicted) when history empties - a failed
    /// rollback must not rotate the id.
    pub fn rollback_last_if_current(
        &self,
        key: &str,
        expected_id: &str,
        expected_role: &str,
        expected_content: &str,
    ) -> std::io::Result<ConditionalSessionWrite> {
        if let Some(backend) = &self.backend {
            let status = backend.remove_last_if_conversation_matches(key, expected_id)?;
            if status == ConditionalSessionWrite::Applied {
                let mut cache = self.cache.lock();
                if let Some(CachedChannelSession::DurableHistory(history)) = cache.get_mut(key) {
                    history.pop();
                }
            }
            return Ok(status);
        }

        let mut cache = self.cache.lock();
        match cache.get_mut(key) {
            Some(CachedChannelSession::Memory(rec)) if rec.conversation_id == expected_id => {
                let should_pop = rec
                    .history
                    .last()
                    .is_some_and(|m| m.role == expected_role && m.content == expected_content);
                if should_pop {
                    rec.history.pop();
                }
                Ok(ConditionalSessionWrite::Applied)
            }
            Some(CachedChannelSession::Memory(_)) => Ok(ConditionalSessionWrite::Stale),
            _ => Ok(ConditionalSessionWrite::Deleted),
        }
    }

    /// Rewrite the history view in place iff the record still carries
    /// `expected_id`, via `compact`. Compaction is a cache-only context-budget
    /// optimization - the backend keeps the full history (matching the
    /// pre-fence behavior). Durable gates on the backend id (`session_exists`
    /// first so resolve never recreates a deleted record); memory-only gates on
    /// the in-cache id. Stale/deleted surfaces as such rather than compacting a
    /// doomed view.
    pub fn compact_history_if_current(
        &self,
        key: &str,
        expected_id: &str,
        compact: impl FnOnce(&mut Vec<ChatMessage>),
    ) -> std::io::Result<ConditionalSessionWrite> {
        if let Some(backend) = &self.backend {
            // Serialize the backend view check/load and cache installation with
            // reset/delete cache invalidation. Lifecycle backend operations do
            // not retain their own lock while waiting for this cache lock.
            let mut cache = self.cache.lock();
            if !backend.session_exists(key) {
                return Ok(ConditionalSessionWrite::Deleted);
            }
            let current = backend.resolve_or_create_conversation_id(key)?;
            if current != expected_id {
                return Ok(ConditionalSessionWrite::Stale);
            }
            let mut messages = match cache.get(key) {
                Some(CachedChannelSession::DurableHistory(history)) => history.clone(),
                _ => backend.load(key),
            };
            compact(&mut messages);
            cache.put(
                key.to_string(),
                CachedChannelSession::DurableHistory(messages),
            );
            return Ok(ConditionalSessionWrite::Applied);
        }

        let mut cache = self.cache.lock();
        match cache.get_mut(key) {
            Some(CachedChannelSession::Memory(rec)) if rec.conversation_id == expected_id => {
                compact(&mut rec.history);
                Ok(ConditionalSessionWrite::Applied)
            }
            Some(CachedChannelSession::Memory(_)) => Ok(ConditionalSessionWrite::Stale),
            _ => Ok(ConditionalSessionWrite::Deleted),
        }
    }

    // ── active-turn lease / lifecycle fencing ──────────────────────────

    /// Register a new in-flight turn for `key` and return a lease. The worker
    /// keeps the lease for the turn's lifetime and calls
    /// [`ChannelSessionState::complete_turn`] on EVERY return path. The
    /// returned [`ChannelTurnLease::cancellation`] is plumbed into the turn
    /// body so a competing reset/delete can cancel it mid-flight.
    pub async fn register_turn(&self, key: &str) -> ChannelTurnLease {
        let id = self.next_turn_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        let completion = Arc::new(TurnCompletion::new());
        let state = ActiveTurnState {
            cancellation: cancellation.clone(),
            completion: Arc::clone(&completion),
        };
        {
            let mut turns = self.active_turns.lock().await;
            let per_key = turns.entry(key.to_string()).or_default();
            per_key.retain(|_, active| !active.completion.is_done());
            per_key.insert(id, state);
        }
        ChannelTurnLease {
            key: key.to_string(),
            id,
            cancellation,
            completion,
        }
    }

    /// Remove the turn from the active map and signal its completion so any
    /// reset/delete waiting on it proceeds. Every worker return path MUST call
    /// this. The active-map lock is released before `mark_done` (sync) so the
    /// signaling never blocks on the map.
    pub async fn complete_turn(&self, lease: &ChannelTurnLease) {
        {
            let mut turns = self.active_turns.lock().await;
            if let Some(per_key) = turns.get_mut(&lease.key) {
                per_key.remove(&lease.id);
                if per_key.is_empty() {
                    turns.remove(&lease.key);
                }
            }
        }
        lease.completion.mark_done();
    }

    /// Cancel every in-flight turn for `key` except `exclude_turn_id`, then
    /// await each one's completion. Used by `reset_session`/`delete_session`
    /// to drain competing workers before mutating. The active-map lock is held
    /// only long enough to COPY the tokens/completions; cancel and await happen
    /// after release so no mutex is held across `.await` and a turn completing
    /// concurrently with the copy still resolves correctly (it is either
    /// copied-then-awaited, or already removed and absent from the copy).
    async fn cancel_and_wait(&self, key: &str, exclude_turn_id: Option<u64>) {
        let to_cancel: Vec<(CancellationToken, Arc<TurnCompletion>)> = {
            let mut turns = self.active_turns.lock().await;
            let mut remove_key = false;
            let to_cancel = turns
                .get_mut(key)
                .map(|per_key| {
                    per_key.retain(|_, state| !state.completion.is_done());
                    remove_key = per_key.is_empty();
                    per_key
                        .iter()
                        .filter(|(id, _)| Some(**id) != exclude_turn_id)
                        .map(|(_, state)| {
                            (state.cancellation.clone(), Arc::clone(&state.completion))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if remove_key {
                turns.remove(key);
            }
            to_cancel
        };
        for (token, _) in &to_cancel {
            token.cancel();
        }
        for (_, completion) in &to_cancel {
            completion.wait().await;
        }
    }

    /// Rotate the record to a fresh id (clearing history), after cancelling and
    /// waiting for every competing turn for `key` except `exclude_turn_id`. The
    /// commanding `/new`/`/clear` turn passes `Some(its own id)` so it never
    /// waits on itself (which would deadlock); gateway API and sessions tools
    /// pass `None`. Returns the fresh id, or the backend error verbatim.
    pub async fn reset_session(
        &self,
        key: &str,
        exclude_turn_id: Option<u64>,
    ) -> std::io::Result<String> {
        self.cancel_and_wait(key, exclude_turn_id).await;
        self.clear_and_rotate_conversation(key)
    }

    /// Remove the record entirely, after cancelling and waiting for every
    /// competing turn for `key` except `exclude_turn_id`. Returns `Ok(false)`
    /// if the record was absent; a subsequent stale-worker expected-id write
    /// then gets `Deleted`. Durable deletes the backend record and drops the
    /// cache entry; memory-only drops the cache entry.
    pub async fn delete_session(
        &self,
        key: &str,
        exclude_turn_id: Option<u64>,
    ) -> std::io::Result<bool> {
        self.cancel_and_wait(key, exclude_turn_id).await;
        let existed = if let Some(backend) = &self.backend {
            backend.delete_session(key)?
        } else {
            self.cache.lock().pop(key).is_some()
        };
        if self.backend.is_some() {
            self.cache.lock().pop(key);
        }
        Ok(existed)
    }

    /// Number of `Memory` records in the cache. Test-only observable for the
    /// "durable mode never creates a Memory record" invariant; not production
    /// API.
    #[cfg(test)]
    fn memory_variant_count_for_test(&self) -> usize {
        self.cache
            .lock()
            .iter()
            .filter(|(_, v)| matches!(v, CachedChannelSession::Memory(_)))
            .count()
    }

    #[cfg(test)]
    async fn active_turn_count_for_test(&self) -> usize {
        self.active_turns
            .lock()
            .await
            .values()
            .map(HashMap::len)
            .sum()
    }

    /// All cached history keys (memory or durable). Test-intended observable so
    /// integration tests in downstream crates that previously iterated the
    /// orchestrator's history map can still enumerate which senders have a
    /// cached view. Kept `pub` (not `cfg(test)`) because downstream test crates
    /// compile this crate in non-test mode; it is a read-only accessor with no
    /// production callers.
    pub fn cached_keys_for_test(&self) -> Vec<String> {
        self.cache.lock().iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Push `message` onto `history` and trim the head beyond `max_history`.
fn push_bounded(history: &mut Vec<ChatMessage>, message: ChatMessage, max_history: usize) {
    history.push(message);
    while history.len() > max_history {
        history.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use crate::session_sqlite::SqliteSessionBackend;
    use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
    use std::sync::mpsc::{Receiver, SyncSender};
    use tempfile::TempDir;

    const BLOCK_NONE: u8 = 0;
    const BLOCK_LOAD: u8 = 1;
    const BLOCK_RESOLVE: u8 = 2;

    struct BlockingBackend {
        inner: Arc<dyn SessionBackend>,
        block_point: AtomicU8,
        reached: SyncSender<()>,
        release: Mutex<Receiver<()>>,
        lifecycle_done: SyncSender<()>,
    }

    impl BlockingBackend {
        fn maybe_block(&self, point: u8) {
            if self
                .block_point
                .compare_exchange(
                    point,
                    BLOCK_NONE,
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Acquire,
                )
                .is_ok()
            {
                self.reached.send(()).unwrap();
                self.release.lock().recv().unwrap();
            }
        }
    }

    impl SessionBackend for BlockingBackend {
        fn load(&self, key: &str) -> Vec<ChatMessage> {
            let messages = self.inner.load(key);
            self.maybe_block(BLOCK_LOAD);
            messages
        }

        fn append(&self, key: &str, message: &ChatMessage) -> std::io::Result<()> {
            self.inner.append(key, message)
        }

        fn remove_last(&self, key: &str) -> std::io::Result<bool> {
            self.inner.remove_last(key)
        }

        fn list_sessions(&self) -> Vec<String> {
            self.inner.list_sessions()
        }

        fn delete_session(&self, key: &str) -> std::io::Result<bool> {
            let result = self.inner.delete_session(key);
            self.lifecycle_done.send(()).unwrap();
            result
        }

        fn session_exists(&self, key: &str) -> bool {
            self.inner.session_exists(key)
        }

        fn resolve_or_create_conversation_id(&self, key: &str) -> std::io::Result<String> {
            let id = self.inner.resolve_or_create_conversation_id(key)?;
            self.maybe_block(BLOCK_RESOLVE);
            Ok(id)
        }

        fn clear_and_rotate_conversation(&self, key: &str) -> std::io::Result<String> {
            let result = self.inner.clear_and_rotate_conversation(key);
            self.lifecycle_done.send(()).unwrap();
            result
        }

        fn append_if_conversation_matches(
            &self,
            key: &str,
            expected_id: &str,
            message: &ChatMessage,
        ) -> std::io::Result<ConditionalSessionWrite> {
            self.inner
                .append_if_conversation_matches(key, expected_id, message)
        }

        fn remove_last_if_conversation_matches(
            &self,
            key: &str,
            expected_id: &str,
        ) -> std::io::Result<ConditionalSessionWrite> {
            self.inner
                .remove_last_if_conversation_matches(key, expected_id)
        }

        fn update_last_if_conversation_matches(
            &self,
            key: &str,
            expected_id: &str,
            message: &ChatMessage,
        ) -> std::io::Result<ConditionalSessionWrite> {
            self.inner
                .update_last_if_conversation_matches(key, expected_id, message)
        }
    }

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
        // Resolve does not touch the cache in durable mode.
        assert_eq!(state.memory_variant_count_for_test(), 0);

        // Loading history caches a DurableHistory view, never a Memory record.
        let _ = state.load_history("linq.main_chat_alice");
        assert_eq!(
            state.memory_variant_count_for_test(),
            0,
            "durable mode must never create a Memory record"
        );
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
            state.memory_variant_count_for_test(),
            1,
            "exactly one entry materialized under contention"
        );
    }

    // ── memory LRU + rollback conditional-write tests ─────────────────

    #[test]
    fn memory_lru_evicts_whole_record_history_and_id_together() {
        // capacity-2 cache: insert A/B, touch A, insert C -> B's WHOLE record is
        // evicted. Re-resolving B mints a fresh id + empty history, never a
        // fresh id + old history.
        let state = ChannelSessionState::with_capacity(None, 2);
        let key_a = "k_a";
        let key_b = "k_b";
        let key_c = "k_c";

        let id_a = state.resolve_conversation_id(key_a).unwrap();
        let id_b = state.resolve_conversation_id(key_b).unwrap();
        state
            .append_history_if_current(key_b, &id_b, ChatMessage::user("b-turn"), 50)
            .unwrap();
        assert_eq!(state.load_history(key_b).len(), 1);

        // Touch A so B becomes LRU.
        assert_eq!(state.resolve_conversation_id(key_a).unwrap(), id_a);
        // Insert C -> evicts B entirely.
        let _ = state.resolve_conversation_id(key_c).unwrap();
        assert!(state.load_history(key_b).is_empty());

        // Re-resolve B: fresh id, empty history.
        let id_b2 = state.resolve_conversation_id(key_b).unwrap();
        assert_ne!(id_b, id_b2, "evicted key must get a fresh id");
        assert!(
            state.load_history(key_b).is_empty(),
            "no old history leaks back"
        );
    }

    #[test]
    fn memory_rollback_keeps_record_and_does_not_rotate_id() {
        let state = ChannelSessionState::new(None);
        let key = "rollback_key";
        let id = state.resolve_conversation_id(key).unwrap();
        state
            .append_history_if_current(key, &id, ChatMessage::user("failed"), 50)
            .unwrap();
        state
            .rollback_last_if_current(key, &id, "user", "failed")
            .unwrap();
        assert!(state.load_history(key).is_empty());
        // The record survives (empty history) and the id is unchanged - a
        // failed rollback must not rotate the id.
        assert_eq!(state.resolve_conversation_id(key).unwrap(), id);
    }

    #[test]
    fn memory_append_after_rotate_is_stale_and_does_not_recreate() {
        let state = ChannelSessionState::new(None);
        let key = "stale_key";
        let id_a = state.resolve_conversation_id(key).unwrap();
        let id_b = state.clear_and_rotate_conversation(key).unwrap();
        assert_ne!(id_a, id_b);

        // A worker holding the old id must see Stale, not Applied, and must not
        // recreate old history.
        assert_eq!(
            state
                .append_history_if_current(key, &id_a, ChatMessage::assistant("stale"), 50)
                .unwrap(),
            ConditionalSessionWrite::Stale
        );
        assert!(state.load_history(key).is_empty());
        // The current id is still id_b.
        assert_eq!(state.resolve_conversation_id(key).unwrap(), id_b);
    }

    #[test]
    fn memory_append_after_delete_is_deleted() {
        let state = ChannelSessionState::new(None);
        let key = "deleted_key";
        let id = state.resolve_conversation_id(key).unwrap();
        // Memory-only delete drops the cache entry.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let existed = runtime.block_on(state.delete_session(key, None)).unwrap();
        assert!(existed);
        assert_eq!(
            state
                .append_history_if_current(key, &id, ChatMessage::assistant("gone"), 50)
                .unwrap(),
            ConditionalSessionWrite::Deleted
        );
        // A genuinely new resolve mints a fresh id (the deleted worker cannot
        // resurrect the old one).
        let id_after = state.resolve_conversation_id(key).unwrap();
        assert_ne!(id, id_after);
    }

    #[tokio::test]
    async fn turn_completion_wait_returns_when_already_done() {
        let completion = TurnCompletion::new();
        completion.mark_done();
        tokio::time::timeout(std::time::Duration::from_secs(1), completion.wait())
            .await
            .expect("late waiter must observe completed turn");
    }

    #[tokio::test]
    async fn aborted_turn_lease_does_not_block_reset_and_is_pruned() {
        let state = Arc::new(ChannelSessionState::new(None));
        let key = "aborted_reset";
        let id_a = state.resolve_conversation_id(key).unwrap();
        let lease = state.register_turn(key).await;
        let entered = Arc::new(Notify::new());
        let entered_task = Arc::clone(&entered);
        let task = zeroclaw_spawn::spawn!(async move {
            let _lease = lease;
            entered_task.notify_one();
            std::future::pending::<()>().await;
        });
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let id_b = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.reset_session(key, None),
        )
        .await
        .expect("aborted lease must not wedge reset")
        .unwrap();
        assert_ne!(id_a, id_b);
        assert_eq!(state.active_turn_count_for_test().await, 0);
    }

    #[tokio::test]
    async fn aborted_turn_lease_does_not_block_delete_and_is_pruned() {
        let state = Arc::new(ChannelSessionState::new(None));
        let key = "aborted_delete";
        state.resolve_conversation_id(key).unwrap();
        let lease = state.register_turn(key).await;
        let task = zeroclaw_spawn::spawn!(async move {
            let _lease = lease;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let existed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.delete_session(key, None),
        )
        .await
        .expect("aborted lease must not wedge delete")
        .unwrap();
        assert!(existed);
        assert_eq!(state.active_turn_count_for_test().await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_load_cache_installation_is_atomic_with_reset_invalidation() {
        let tmp = TempDir::new().unwrap();
        let inner: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
        let key = "durable_load_reset";
        let id_a = inner.resolve_or_create_conversation_id(key).unwrap();
        inner
            .append_if_conversation_matches(key, &id_a, &ChatMessage::user("A"))
            .unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (lifecycle_tx, lifecycle_rx) = std::sync::mpsc::sync_channel(1);
        let backend_impl = Arc::new(BlockingBackend {
            inner,
            block_point: AtomicU8::new(BLOCK_LOAD),
            reached: reached_tx,
            release: Mutex::new(release_rx),
            lifecycle_done: lifecycle_tx,
        });
        let backend: Arc<dyn SessionBackend> = backend_impl;
        let state = Arc::new(ChannelSessionState::new(Some(backend)));

        let loader_state = Arc::clone(&state);
        let loader = tokio::task::spawn_blocking(move || loader_state.load_history(key));
        tokio::task::spawn_blocking(move || reached_rx.recv().unwrap())
            .await
            .unwrap();
        let reset_state = Arc::clone(&state);
        let reset =
            zeroclaw_spawn::spawn!(async move { reset_state.reset_session(key, None).await });
        tokio::task::spawn_blocking(move || lifecycle_rx.recv().unwrap())
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(loader.await.unwrap()[0].content, "A");
        let id_b = reset.await.unwrap().unwrap();
        assert_ne!(id_a, id_b);
        assert!(state.load_history(key).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_compaction_cache_installation_is_atomic_with_reset_invalidation() {
        let tmp = TempDir::new().unwrap();
        let inner: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
        let key = "durable_compact_reset";
        let id_a = inner.resolve_or_create_conversation_id(key).unwrap();
        inner
            .append_if_conversation_matches(key, &id_a, &ChatMessage::user("A"))
            .unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (lifecycle_tx, lifecycle_rx) = std::sync::mpsc::sync_channel(1);
        let backend_impl = Arc::new(BlockingBackend {
            inner,
            block_point: AtomicU8::new(BLOCK_RESOLVE),
            reached: reached_tx,
            release: Mutex::new(release_rx),
            lifecycle_done: lifecycle_tx,
        });
        let backend: Arc<dyn SessionBackend> = backend_impl;
        let state = Arc::new(ChannelSessionState::new(Some(backend)));

        let compact_state = Arc::clone(&state);
        let expected = id_a.clone();
        let compact = tokio::task::spawn_blocking(move || {
            compact_state.compact_history_if_current(key, &expected, |_| {})
        });
        tokio::task::spawn_blocking(move || reached_rx.recv().unwrap())
            .await
            .unwrap();
        let reset_state = Arc::clone(&state);
        let reset =
            zeroclaw_spawn::spawn!(async move { reset_state.reset_session(key, None).await });
        tokio::task::spawn_blocking(move || lifecycle_rx.recv().unwrap())
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(
            compact.await.unwrap().unwrap(),
            ConditionalSessionWrite::Applied
        );
        let id_b = reset.await.unwrap().unwrap();
        assert_ne!(id_a, id_b);
        assert!(state.load_history(key).is_empty());
    }

    #[test]
    fn memory_compact_is_conditional_on_id() {
        let state = ChannelSessionState::new(None);
        let key = "compact_key";
        let id = state.resolve_conversation_id(key).unwrap();
        for content in ["one", "two", "three", "four"] {
            state
                .append_history_if_current(key, &id, ChatMessage::user(content), 50)
                .unwrap();
        }
        assert_eq!(state.load_history(key).len(), 4);

        // Compaction with the current id keeps the last 2 messages.
        state
            .compact_history_if_current(key, &id, |history| {
                let keep = history.len().saturating_sub(2);
                history.drain(0..keep);
            })
            .unwrap();
        let compacted = state.load_history(key);
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].content, "three");
        assert_eq!(compacted[1].content, "four");

        // Compaction with a stale id is Stale and does not mutate.
        let stale = uuid::Uuid::new_v4().to_string();
        let status = state
            .compact_history_if_current(key, &stale, |_| {})
            .unwrap();
        assert_eq!(status, ConditionalSessionWrite::Stale);
        assert_eq!(state.load_history(key).len(), 2);
    }
}
