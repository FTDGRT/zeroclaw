//! JSONL-based session persistence for channel conversations.

use crate::session_backend::SessionBackend;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use zeroclaw_api::model_provider::ChatMessage;
pub use zeroclaw_api::session_keys::sanitize_session_key;

type MutationLock = parking_lot::Mutex<()>;

static MUTATION_LOCKS: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Weak<MutationLock>>>> =
    OnceLock::new();

/// Suffix for the per-key advisory file lock (held across resolve / rotate /
/// delete so two `SessionStore` instances on the same dir converge on one
/// conversation id). Kept distinct from `.jsonl` so it never shows up in
/// `list_sessions`.
const LOCK_SUFFIX: &str = ".lock";
/// Suffix for the conversation-identity sidecar persisted next to `.jsonl`.
const META_SUFFIX: &str = ".meta.json";

/// Append-only JSONL session store for channel conversations.
pub struct SessionStore {
    sessions_dir: PathBuf,
    mutation_lock: Arc<MutationLock>,
}

impl SessionStore {
    /// Create a new session store, ensuring the sessions directory exists.
    pub fn new(workspace_dir: &Path) -> std::io::Result<Self> {
        let sessions_dir = workspace_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        let mutation_lock = mutation_lock_for(&sessions_dir)?;
        Ok(Self {
            sessions_dir,
            mutation_lock,
        })
    }

    /// Compute the file path for a session key, sanitizing for filesystem safety.
    fn session_path(&self, session_key: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.jsonl", sanitize_session_key(session_key)))
    }

    /// Path to the per-key advisory lock file. Derives from the sanitized key
    /// the same way `.jsonl` does so the two stay siblings.
    fn lock_path(&self, session_key: &str) -> PathBuf {
        self.sessions_dir.join(format!(
            "{}{}",
            sanitize_session_key(session_key),
            LOCK_SUFFIX
        ))
    }

    /// Path to the conversation-identity sidecar (`{"conversation_id": "..."}`).
    fn meta_path(&self, session_key: &str) -> PathBuf {
        self.sessions_dir.join(format!(
            "{}{}",
            sanitize_session_key(session_key),
            META_SUFFIX
        ))
    }

    /// Run `f` while holding the per-key exclusive file lock. Creates the
    /// `.lock` file if it does not yet exist. The lock is advisory and
    /// process-local-coherent: it serializes resolve / clear+rotate / delete
    /// across independent `SessionStore` instances pointing at the same dir,
    /// which is what makes concurrent first-access converge on one id.
    #[allow(clippy::suspicious_open_options)]
    fn with_key_lock<R>(
        &self,
        session_key: &str,
        f: impl FnOnce() -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let lock_path = self.lock_path(session_key);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Lock file content is irrelevant - only the file lock is used - so
        // neither truncate nor append is wanted here.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        file.lock()?;
        f()
    }

    /// Read the persisted conversation id from the sidecar. `None` if the
    /// sidecar is absent (legacy `.jsonl` with no identity yet) or holds an
    /// empty value.
    fn read_conversation_id(&self, session_key: &str) -> std::io::Result<Option<String>> {
        let path = self.meta_path(session_key);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let v: serde_json::Value = serde_json::from_str(&contents)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(v.get("conversation_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .filter(|s| !s.is_empty()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Persist the conversation id to the sidecar via a sibling temp file +
    /// flush/sync + atomic rename, so a crash mid-write never leaves a
    /// truncated or empty identity. Caller already holds the per-key lock.
    fn write_conversation_id(&self, session_key: &str, id: &str) -> std::io::Result<()> {
        let path = self.meta_path(session_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::json!({ "conversation_id": id }).to_string() + "\n";
        let tmp = path.with_extension("tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load all messages for a session from its JSONL file.
    /// Returns an empty vec if the file does not exist or is unreadable.
    pub fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        let path = self.session_path(session_key);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(trimmed) {
                messages.push(msg);
            }
        }

        messages
    }

    /// Append a single message to the session JSONL file. Runs under the
    /// same per-key exclusive lock as resolve/rotate/delete so a concurrent
    /// append can never interleave with a `/clear` truncation mid-write.
    pub fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
        let _guard = self.mutation_lock.lock();
        self.with_key_lock(session_key, || {
            let path = self.session_path(session_key);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;

            let json = serde_json::to_string(message)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

            writeln!(file, "{json}")?;
            Ok(())
        })
    }

    /// Remove the last message from a session's JSONL file.
    /// Rewrite approach: load all messages, drop the last, rewrite. This is
    /// O(n) but rollbacks are rare.
    pub fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        let _guard = self.mutation_lock.lock();
        let mut messages = self.load(session_key);
        if messages.is_empty() {
            return Ok(false);
        }
        messages.pop();
        self.rewrite(session_key, &messages)?;
        Ok(true)
    }

    /// Replace the last message without exposing an intermediate truncated session.
    pub fn update_last(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<bool> {
        self.update_last_with(session_key, message, |temp, path| {
            temp.persist(path).map(|_| ()).map_err(|error| error.error)
        })
    }

    fn update_last_with<F>(
        &self,
        session_key: &str,
        message: &ChatMessage,
        persist: F,
    ) -> std::io::Result<bool>
    where
        F: FnOnce(tempfile::NamedTempFile, &Path) -> std::io::Result<()>,
    {
        let _guard = self.mutation_lock.lock();
        let mut messages = self.load(session_key);
        let Some(last) = messages.last_mut() else {
            return Ok(false);
        };
        *last = message.clone();
        self.rewrite_with(session_key, &messages, persist)?;
        Ok(true)
    }

    /// Compact a session file by rewriting only valid messages (removes corrupt lines).
    pub fn compact(&self, session_key: &str) -> std::io::Result<()> {
        let _guard = self.mutation_lock.lock();
        let messages = self.load(session_key);
        self.rewrite(session_key, &messages)
    }

    fn rewrite(&self, session_key: &str, messages: &[ChatMessage]) -> std::io::Result<()> {
        self.rewrite_with(session_key, messages, |temp, path| {
            temp.persist(path).map(|_| ()).map_err(|error| error.error)
        })
    }

    fn rewrite_with<F>(
        &self,
        session_key: &str,
        messages: &[ChatMessage],
        persist: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(tempfile::NamedTempFile, &Path) -> std::io::Result<()>,
    {
        let path = self.session_path(session_key);
        let mut temp = tempfile::NamedTempFile::new_in(&self.sessions_dir)?;
        for msg in messages {
            serde_json::to_writer(&mut temp, msg)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            temp.write_all(b"\n")?;
        }

        temp.as_file().sync_all()?;
        persist(temp, &path)
    }

    /// Clear all messages from a session by truncating its JSONL file.
    /// The file is preserved (empty) so the session key remains in `list_sessions`.
    pub fn clear_messages(&self, session_key: &str) -> std::io::Result<usize> {
        let _guard = self.mutation_lock.lock();
        let count = self.load(session_key).len();
        if count > 0 {
            self.rewrite(session_key, &[])?;
        }
        Ok(count)
    }

    /// Delete a session's JSONL file and conversation-identity sidecar.
    /// Returns `true` if either existed. The per-key `.lock` file is
    /// intentionally NOT unlinked so concurrent operations on the same key
    /// stay coherent. The delete is performed under the per-key exclusive
    /// lock so it cannot race a concurrent resolve / rotate.
    pub fn delete_session(&self, session_key: &str) -> std::io::Result<bool> {
        let _guard = self.mutation_lock.lock();
        self.with_key_lock(session_key, || {
            let data_path = self.session_path(session_key);
            let meta_path = self.meta_path(session_key);
            let data_existed = data_path.exists();
            let meta_existed = meta_path.exists();
            // Remove the identity sidecar FIRST and propagate its error. A
            // silently-ignored sidecar removal (`let _ = ...`) would leave the
            // old conversation_id on disk after the data file is gone, so the
            // next resolve would read back and REUSE a "deleted" id - exactly
            // the identity-leak the review flagged. Ordering sidecar-before-data
            // means a failure here aborts before we touch history, leaving a
            // coherent "both still present" state rather than "history gone,
            // stale id survives".
            if meta_existed {
                std::fs::remove_file(&meta_path)?;
            }
            if data_existed {
                std::fs::remove_file(&data_path)?;
            }
            Ok(data_existed || meta_existed)
        })
    }

    /// Return the modification time of a session's JSONL file.
    pub fn session_mtime(&self, session_key: &str) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.session_path(session_key))
            .and_then(|m| m.modified())
            .ok()
    }

    /// List all session keys that have files on disk.
    pub fn list_sessions(&self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().into_string().ok()?;
                name.strip_suffix(".jsonl").map(String::from)
            })
            .collect()
    }
}

fn mutation_lock_for(sessions_dir: &Path) -> std::io::Result<Arc<MutationLock>> {
    let key = sessions_dir.canonicalize()?;
    let registry = MUTATION_LOCKS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    let mut locks = registry.lock();
    locks.retain(|_, lock| lock.strong_count() > 0);

    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }

    let lock = Arc::new(MutationLock::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

impl SessionBackend for SessionStore {
    fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        self.load(session_key)
    }

    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
        self.append(session_key, message)
    }

    fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        self.remove_last(session_key)
    }

    fn update_last(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<bool> {
        self.update_last(session_key, message)
    }

    fn list_sessions(&self) -> Vec<String> {
        self.list_sessions()
    }

    fn list_sessions_with_metadata(&self) -> Vec<crate::session_backend::SessionMetadata> {
        use chrono::{DateTime, Utc};
        self.list_sessions()
            .into_iter()
            .map(|key| {
                let last_activity: DateTime<Utc> = self
                    .session_mtime(&key)
                    .map(DateTime::<Utc>::from)
                    .unwrap_or_else(Utc::now);
                crate::session_backend::SessionMetadata {
                    name: None,
                    created_at: last_activity,
                    last_activity,
                    message_count: 0,
                    key,
                    agent_alias: None,
                    channel_id: None,
                    room_id: None,
                    sender_id: None,
                    // The listing is intentionally partial (mirrors `name` /
                    // `message_count` being best-effort here). The
                    // authoritative read is `resolve_or_create_conversation_id`.
                    conversation_id: None,
                }
            })
            .collect()
    }

    fn compact(&self, session_key: &str) -> std::io::Result<()> {
        self.compact(session_key)
    }

    fn clear_messages(&self, session_key: &str) -> std::io::Result<usize> {
        self.clear_messages(session_key)
    }

    fn delete_session(&self, session_key: &str) -> std::io::Result<bool> {
        self.delete_session(session_key)
    }

    /// Quick existence probe mirroring how `delete_session` decides whether
    /// the session is on disk Checking file presence is the same
    /// O(1) `stat` that `delete_session` itself performs.
    fn session_exists(&self, session_key: &str) -> bool {
        self.session_path(session_key).exists()
    }

    /// Atomically resolve-or-create the conversation id for a session key.
    /// Reads the `.meta.json` sidecar under the per-key exclusive lock; if
    /// absent/empty (legacy `.jsonl` with no sidecar, or a brand new key) it
    /// generates a UUID and persists it via temp+sync+rename. The exclusive
    /// lock makes two `SessionStore` instances on the same dir converge on a
    /// single id.
    fn resolve_or_create_conversation_id(&self, session_key: &str) -> std::io::Result<String> {
        self.with_key_lock(session_key, || {
            if let Some(existing) = self.read_conversation_id(session_key)? {
                return Ok(existing);
            }
            let id = uuid::Uuid::new_v4().to_string();
            self.write_conversation_id(session_key, &id)?;
            Ok(id)
        })
    }

    /// Atomically clear the JSONL history AND rotate the conversation id in
    /// one record-scoped operation under the per-key exclusive lock. The
    /// `.jsonl` is truncated (file preserved so the key stays listed) and a
    /// fresh UUID is persisted to the sidecar. This is the `/new`/`/clear`
    /// path - `remove_last`, `update_last`, `compact`, and crash repair do
    /// NOT rotate.
    fn clear_and_rotate_conversation(&self, session_key: &str) -> std::io::Result<String> {
        self.with_key_lock(session_key, || {
            let data_path = self.session_path(session_key);
            if let Some(parent) = data_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Write the fresh id FIRST (via write_conversation_id's own
            // temp+sync+rename, so this step alone is crash-atomic), THEN
            // truncate history. A crash between the two steps leaves
            // "new id + stale history" (the FULL old history may survive
            // because the truncate below has not been fsynced yet) rather
            // than "empty history + stale id" (a `/clear` that silently
            // didn't rotate). Both steps run under the same per-key exclusive
            // lock as before.
            let id = uuid::Uuid::new_v4().to_string();
            self.write_conversation_id(session_key, &id)?;
            // Truncate the history. `File::create` truncates to empty while
            // preserving the file so the key remains in `list_sessions`.
            // fsync so the truncate is durable on power loss (returning Ok
            // alone is not a durability guarantee).
            {
                let file = std::fs::File::create(&data_path)?;
                file.sync_all()?;
            }
            Ok(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn round_trip_append_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store
            .append("telegram_user123", &ChatMessage::user("hello"))
            .unwrap();
        store
            .append("telegram_user123", &ChatMessage::assistant("hi there"))
            .unwrap();

        let messages = store.load("telegram_user123");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "hi there");
    }

    #[test]
    fn load_nonexistent_session_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        let messages = store.load("nonexistent");
        assert!(messages.is_empty());
    }

    #[test]
    fn key_sanitization() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store
            .append("slack/thread:123/user", &ChatMessage::user("test"))
            .unwrap();

        let messages = store.load("slack/thread:123/user");
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn sanitize_session_key_is_idempotent() {
        let raw = "slack_C123_1.2_user one";
        let once = sanitize_session_key(raw);
        let twice = sanitize_session_key(&once);
        assert_eq!(once, "slack_C123_1_2_user_one");
        assert_eq!(once, twice);
    }

    #[test]
    fn restart_simulation_matches_when_caller_pre_sanitizes() {
        let tmp = TempDir::new().unwrap();
        let runtime_key = sanitize_session_key("slack_C123_1.2_user one");

        {
            let store = SessionStore::new(tmp.path()).unwrap();
            store
                .append(&runtime_key, &ChatMessage::user("first"))
                .unwrap();
            store
                .append(&runtime_key, &ChatMessage::assistant("ack"))
                .unwrap();
        }

        let store = SessionStore::new(tmp.path()).unwrap();
        let listed = store.list_sessions();
        assert_eq!(listed, vec![runtime_key.clone()]);

        let msgs = store.load(&listed[0]);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "first");
        assert_eq!(msgs[1].content, "ack");
    }

    #[test]
    fn list_sessions_returns_keys() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store
            .append("telegram_alice", &ChatMessage::user("hi"))
            .unwrap();
        store
            .append("discord_bob", &ChatMessage::user("hey"))
            .unwrap();

        let mut sessions = store.list_sessions();
        sessions.sort();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&"discord_bob".to_string()));
        assert!(sessions.contains(&"telegram_alice".to_string()));
    }

    #[test]
    fn append_is_truly_append_only() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "test_session";

        store.append(key, &ChatMessage::user("msg1")).unwrap();
        store.append(key, &ChatMessage::user("msg2")).unwrap();

        // Read raw file to verify append-only format
        let path = store.session_path(key);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn remove_last_drops_final_message() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store
            .append("rm_test", &ChatMessage::user("first"))
            .unwrap();
        store
            .append("rm_test", &ChatMessage::user("second"))
            .unwrap();

        assert!(store.remove_last("rm_test").unwrap());
        let messages = store.load("rm_test");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "first");
    }

    #[test]
    fn remove_last_empty_returns_false() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        assert!(!store.remove_last("nonexistent").unwrap());
    }

    #[test]
    fn update_last_via_trait_replaces_final_message() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;
        let key = "update_test";

        backend.append(key, &ChatMessage::user("first")).unwrap();
        backend.append(key, &ChatMessage::assistant("old")).unwrap();

        assert!(
            backend
                .update_last(key, &ChatMessage::assistant("new"))
                .unwrap()
        );

        let messages = backend.load(key);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "first");
        assert_eq!(messages[1].content, "new");
    }

    #[test]
    fn failed_rewrite_preserves_original_file() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "rewrite_failure";

        store.append(key, &ChatMessage::user("first")).unwrap();
        store
            .append(key, &ChatMessage::assistant("second"))
            .unwrap();
        let path = store.session_path(key);
        let original = std::fs::read(&path).unwrap();

        let mut temp_path = None;
        let result = store.rewrite_with(key, &[ChatMessage::user("replacement")], |temp, _path| {
            temp_path = Some(temp.path().to_path_buf());
            Err(std::io::Error::other("injected persist failure"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!temp_path.unwrap().exists());
    }

    #[test]
    fn concurrent_append_waits_for_update_last_commit() {
        let tmp = TempDir::new().unwrap();
        let update_store = Arc::new(SessionStore::new(tmp.path()).unwrap());
        let append_store = Arc::new(SessionStore::new(tmp.path()).unwrap());
        let key = "concurrent_update";
        update_store
            .append(key, &ChatMessage::user("first"))
            .unwrap();
        update_store
            .append(key, &ChatMessage::assistant("old"))
            .unwrap();

        let (staged_tx, staged_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let update_worker = Arc::clone(&update_store);
        let updater = std::thread::spawn(move || {
            update_worker.update_last_with(key, &ChatMessage::assistant("new"), |temp, path| {
                staged_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                temp.persist(path).map(|_| ()).map_err(|error| error.error)
            })
        });

        staged_rx.recv().unwrap();
        let (append_started_tx, append_started_rx) = mpsc::channel();
        let (append_done_tx, append_done_rx) = mpsc::channel();
        let append_store = Arc::clone(&append_store);
        let appender = std::thread::spawn(move || {
            append_started_tx.send(()).unwrap();
            let result = append_store.append(key, &ChatMessage::user("concurrent"));
            append_done_tx.send(()).unwrap();
            result
        });

        append_started_rx.recv().unwrap();
        assert!(
            append_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        release_tx.send(()).unwrap();
        assert!(updater.join().unwrap().unwrap());
        appender.join().unwrap().unwrap();

        let messages = update_store.load(key);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "first");
        assert_eq!(messages[1].content, "new");
        assert_eq!(messages[2].content, "concurrent");
    }

    #[test]
    fn compact_removes_corrupt_lines() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "compact_test";

        let path = store.session_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, r#"{{"role":"user","content":"ok"}}"#).unwrap();
        writeln!(file, "corrupt line").unwrap();
        writeln!(file, r#"{{"role":"assistant","content":"hi"}}"#).unwrap();

        store.compact(key).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.trim().lines().count(), 2);
    }

    #[test]
    fn session_backend_trait_works_via_dyn() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;

        backend
            .append("trait_test", &ChatMessage::user("hello"))
            .unwrap();
        let msgs = backend.load("trait_test");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn handles_corrupt_lines_gracefully() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "corrupt_test";

        // Write valid message + corrupt line + valid message
        let path = store.session_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, r#"{{"role":"user","content":"hello"}}"#).unwrap();
        writeln!(file, "this is not valid json").unwrap();
        writeln!(file, r#"{{"role":"assistant","content":"world"}}"#).unwrap();

        let messages = store.load(key);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "world");
    }

    #[test]
    fn clear_messages_truncates_file() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "clear_test";

        store.append(key, &ChatMessage::user("hello")).unwrap();
        store.append(key, &ChatMessage::assistant("world")).unwrap();

        let cleared = store.clear_messages(key).unwrap();
        assert_eq!(cleared, 2);
        assert!(store.load(key).is_empty());
        // File still exists — session key remains in list_sessions
        assert!(store.session_path(key).exists());
    }

    #[test]
    fn clear_messages_empty_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        assert_eq!(store.clear_messages("nonexistent").unwrap(), 0);
    }

    #[test]
    fn clear_messages_does_not_affect_other_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store
            .append("alice", &ChatMessage::user("alice msg"))
            .unwrap();
        store.append("bob", &ChatMessage::user("bob msg")).unwrap();

        store.clear_messages("alice").unwrap();
        assert!(store.load("alice").is_empty());
        assert_eq!(store.load("bob").len(), 1);
    }

    #[test]
    fn clear_messages_then_append_works() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "reuse_test";

        store.append(key, &ChatMessage::user("old")).unwrap();
        store.clear_messages(key).unwrap();
        store.append(key, &ChatMessage::user("new")).unwrap();

        let messages = store.load(key);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "new");
    }

    #[test]
    fn delete_session_removes_jsonl_file() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let key = "delete_test";

        store.append(key, &ChatMessage::user("hello")).unwrap();
        assert_eq!(store.load(key).len(), 1);

        let deleted = store.delete_session(key).unwrap();
        assert!(deleted);
        assert!(store.load(key).is_empty());
        assert!(!store.session_path(key).exists());
    }

    #[test]
    fn delete_session_nonexistent_returns_false() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        let deleted = store.delete_session("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn delete_session_via_trait() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;

        backend
            .append("trait_delete", &ChatMessage::user("hello"))
            .unwrap();
        assert_eq!(backend.load("trait_delete").len(), 1);

        let deleted = backend.delete_session("trait_delete").unwrap();
        assert!(deleted);
        assert!(backend.load("trait_delete").is_empty());
    }

    // ── session_exists─────────────────────────────────────
    #[test]
    fn session_exists_tracks_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;

        assert!(!backend.session_exists("ghost"));

        backend
            .append("ghost", &ChatMessage::user("first"))
            .unwrap();
        assert!(backend.session_exists("ghost"));

        backend.delete_session("ghost").unwrap();
        assert!(!backend.session_exists("ghost"));
    }

    // ── get_session_metadata (trait default) tests ──────────────────

    #[test]
    fn get_session_metadata_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;
        assert!(backend.get_session_metadata("nonexistent").is_none());
    }

    #[test]
    fn get_session_metadata_returns_correct_count() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend: &dyn SessionBackend = &store;

        backend
            .append("test_session", &ChatMessage::user("hello"))
            .unwrap();
        backend
            .append("test_session", &ChatMessage::assistant("hi"))
            .unwrap();

        let meta = backend.get_session_metadata("test_session").unwrap();
        assert_eq!(meta.key, "test_session");
        assert_eq!(meta.message_count, 2);
        assert!(meta.name.is_none());
    }

    // ── conversation_id (atomic channel identity) tests ───────────────

    #[test]
    fn conversation_id_resolve_is_idempotent_jsonl() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        let id1 = store.resolve_or_create_conversation_id("k").unwrap();
        let id2 = store.resolve_or_create_conversation_id("k").unwrap();
        assert!(!id1.is_empty());
        assert_eq!(id1, id2, "repeated resolve must return the same id");
        // Sidecar is written next to the jsonl.
        assert!(store.meta_path("k").exists());
    }

    #[test]
    fn conversation_id_legacy_jsonl_backfills_sidecar() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        // Legacy: a `.jsonl` exists with NO sidecar (pre-dates the identity
        // column). First resolve must synthesize the sidecar, not fail.
        store.append("legacy", &ChatMessage::user("old")).unwrap();
        assert!(!store.meta_path("legacy").exists(), "legacy has no sidecar");

        let id = store.resolve_or_create_conversation_id("legacy").unwrap();
        assert!(!id.is_empty());
        assert!(
            store.meta_path("legacy").exists(),
            "resolve must create sidecar"
        );
        assert_eq!(
            store.resolve_or_create_conversation_id("legacy").unwrap(),
            id,
            "re-resolve returns the same id"
        );
    }

    #[test]
    fn conversation_id_survives_reopen_jsonl() {
        let tmp = TempDir::new().unwrap();
        let id_before = {
            let store = SessionStore::new(tmp.path()).unwrap();
            store.resolve_or_create_conversation_id("persist").unwrap()
        };
        let store2 = SessionStore::new(tmp.path()).unwrap();
        let id_after = store2.resolve_or_create_conversation_id("persist").unwrap();
        assert_eq!(id_before, id_after);
    }

    #[test]
    fn conversation_id_clear_and_rotate_clears_history_and_mints_new_id_jsonl() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store.append("rot", &ChatMessage::user("a")).unwrap();
        store.append("rot", &ChatMessage::assistant("b")).unwrap();
        let id1 = store.resolve_or_create_conversation_id("rot").unwrap();
        assert_eq!(store.load("rot").len(), 2);

        let id2 = store.clear_and_rotate_conversation("rot").unwrap();
        assert_ne!(id1, id2, "rotate must mint a fresh id");
        assert!(store.load("rot").is_empty(), "rotate must clear history");
        // The .jsonl is preserved (truncated) so the key stays listed.
        assert!(store.session_path("rot").exists());
        // The sidecar now holds the rotated id.
        let stored = store.read_conversation_id("rot").unwrap();
        assert_eq!(stored.as_deref(), Some(id2.as_str()));
        // Post-rotate resolve is stable on the new id.
        assert_eq!(store.resolve_or_create_conversation_id("rot").unwrap(), id2);
    }

    #[test]
    fn conversation_id_other_key_isolation_jsonl() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        let id_a = store.resolve_or_create_conversation_id("a").unwrap();
        let id_b = store.resolve_or_create_conversation_id("b").unwrap();
        assert_ne!(id_a, id_b);

        let id_a2 = store.clear_and_rotate_conversation("a").unwrap();
        assert_ne!(id_a, id_a2);
        assert_eq!(
            store.resolve_or_create_conversation_id("b").unwrap(),
            id_b,
            "other-key isolation: rotate(a) must not change b"
        );
    }

    #[test]
    fn conversation_id_delete_then_recreate_mints_new_id_jsonl() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        // Seed a data file + sidecar so both exist before delete.
        store.append("del", &ChatMessage::user("x")).unwrap();
        let id1 = store.resolve_or_create_conversation_id("del").unwrap();
        assert!(store.delete_session("del").unwrap());
        assert!(
            !store.meta_path("del").exists(),
            "delete must remove sidecar"
        );
        let id2 = store.resolve_or_create_conversation_id("del").unwrap();
        assert_ne!(id1, id2, "delete + recreate must mint a fresh id");
    }

    #[test]
    fn conversation_id_concurrent_resolve_converges_jsonl() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        // Two independent SessionStore instances on the same dir. The
        // per-key file lock must serialize them onto one id.
        let a = Arc::new(SessionStore::new(tmp.path()).unwrap());
        let b = SessionStore::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let key = "conv_concurrent";

        let bar = barrier.clone();
        let a_c = a.clone();
        let h1 = thread::spawn(move || {
            bar.wait();
            a_c.resolve_or_create_conversation_id(key).unwrap()
        });
        let bar2 = barrier.clone();
        let h2 = thread::spawn(move || {
            bar2.wait();
            b.resolve_or_create_conversation_id(key).unwrap()
        });
        let id1 = h1.join().unwrap();
        let id2 = h2.join().unwrap();

        assert!(!id1.is_empty() && !id2.is_empty());
        assert_eq!(
            id1, id2,
            "two concurrent first-access resolves must converge on one id"
        );

        // A third fresh instance reads the same persisted id.
        let c = SessionStore::new(tmp.path()).unwrap();
        assert_eq!(c.resolve_or_create_conversation_id(key).unwrap(), id1);
    }

    #[test]
    fn conversation_id_resolve_and_rotate_race_stays_consistent_jsonl() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let a = Arc::new(SessionStore::new(tmp.path()).unwrap());
        let initial = a.resolve_or_create_conversation_id("race").unwrap();
        let b = SessionStore::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let bar = barrier.clone();
        let a_c = a.clone();
        let h_res = thread::spawn(move || {
            bar.wait();
            let mut ids = Vec::new();
            for _ in 0..64 {
                ids.push(a_c.resolve_or_create_conversation_id("race").unwrap());
            }
            ids
        });

        let bar2 = barrier.clone();
        let h_rot = thread::spawn(move || {
            bar2.wait();
            b.clear_and_rotate_conversation("race").unwrap()
        });

        let rotated = h_rot.join().unwrap();
        let ids = h_res.join().unwrap();
        assert_ne!(rotated, initial);
        for id in &ids {
            assert!(!id.is_empty(), "race produced an empty id");
            assert!(
                *id == initial || *id == rotated,
                "race produced an id ({id}) that is neither the pre- nor post-rotate value"
            );
        }

        // After both threads joined the rotate has committed. A fresh
        // instance must observe post-rotate state.
        let c = SessionStore::new(tmp.path()).unwrap();
        assert_eq!(
            c.resolve_or_create_conversation_id("race").unwrap(),
            rotated,
            "final persisted id must be the rotated one"
        );
        assert!(
            c.load("race").is_empty(),
            "rotate must have cleared history"
        );
    }

    // ── crash / delete hardening tests ───────────────────────────────

    #[test]
    fn clear_and_rotate_mints_fresh_id_and_truncates() {
        // Asserts the observable post-state (new id persisted + history
        // cleared), NOT crash injection; the write-id-before-truncate ORDER
        // is statically guaranteed by the implementation structure -
        // `write_conversation_id` is a complete temp+sync+rename atomic write
        // that completes before `File::create` truncates, and
        // `file.sync_all()` provides truncate durability - so no
        // fault-injection seam is needed in production code.
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        store.append("rot", &ChatMessage::user("a")).unwrap();
        store.append("rot", &ChatMessage::assistant("b")).unwrap();
        let id_before = store.resolve_or_create_conversation_id("rot").unwrap();
        assert_eq!(store.load("rot").len(), 2);

        let id_after = store.clear_and_rotate_conversation("rot").unwrap();
        assert_ne!(id_after, id_before, "rotate must mint a fresh id");
        assert!(store.load("rot").is_empty(), "history must be truncated");
        assert_eq!(
            store.resolve_or_create_conversation_id("rot").unwrap(),
            id_after,
            "fresh id must be persisted to the sidecar"
        );
    }

    #[test]
    fn delete_session_propagates_sidecar_removal_error() {
        // Inject a REAL failure: replace the sidecar FILE with a DIRECTORY at
        // the same path. `remove_file` on a directory returns `Err`
        // cross-platform (no permission bits needed), so the sidecar-removal
        // step inside `delete_session` is forced to fail. Because sidecar
        // removal now runs BEFORE data removal and propagates its error,
        // `delete_session` must return `Err` and the data file must STILL be
        // present (the failure aborted before touching history).
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        // Seed a data file + identity sidecar so both exist.
        store.append("del", &ChatMessage::user("x")).unwrap();
        let _ = store.resolve_or_create_conversation_id("del").unwrap();
        let meta_path = store.meta_path("del");
        let data_path = store.session_path("del");
        assert!(meta_path.exists(), "sidecar must exist after resolve");
        assert!(data_path.exists(), "data must exist after append");

        // Swap the sidecar file for a directory so `remove_file` fails.
        std::fs::remove_file(&meta_path).unwrap();
        std::fs::create_dir(&meta_path).unwrap();

        let result = store.delete_session("del");
        assert!(
            result.is_err(),
            "delete must propagate sidecar-removal failure"
        );
        assert!(
            data_path.exists(),
            "data file must survive when sidecar removal fails first"
        );
    }
}
