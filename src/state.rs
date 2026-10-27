//! Small app-level state presage doesn't track: read markers, the storage-service chat list
//! and disappearing-message countdowns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::store::Thread;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
pub struct AppState {
    /// thread key -> newest sent-timestamp the user has seen.
    last_read: HashMap<String, u64>,
    /// Chats the storage service says belong in the list even without local messages.
    #[serde(default)]
    pub listed: HashSet<String>,
    /// Profile names from the storage service, for people outside the address book.
    #[serde(default)]
    pub profile_names: HashMap<String, String>,
    /// Disappearing messages whose countdown has started.
    #[serde(default)]
    pub expiring: Vec<Expiry>,
    /// Recently sent stickers, newest first: (pack id, sticker id).
    #[serde(default)]
    pub recent_stickers: Vec<(Vec<u8>, u32)>,
    #[serde(skip)]
    path: PathBuf,
}

/// One message's countdown; Signal identifies a message by thread + author + sent timestamp.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Expiry {
    pub thread: String,
    pub author: Uuid,
    pub ts: u64,
    pub expires_at: u64,
}

/// When a countdown started at `start_ms` with a `timer_s` timer runs out.
pub fn expires_at(start_ms: u64, timer_s: u32) -> u64 {
    start_ms + u64::from(timer_s) * 1000
}

/// Seconds left, rounded up so 0 only means gone.
pub fn remaining_s(expires_at: u64, now_ms: u64) -> u64 {
    expires_at.saturating_sub(now_ms).div_ceil(1000)
}

/// Signal Desktop's recents row holds this many.
pub const RECENT_STICKERS: usize = 24;

/// Moves `sticker` to the front of `recent`, dropping the oldest past the cap.
pub fn push_recent(recent: &mut Vec<(Vec<u8>, u32)>, sticker: (Vec<u8>, u32)) {
    recent.retain(|s| *s != sticker);
    recent.insert(0, sticker);
    recent.truncate(RECENT_STICKERS);
}

/// Inverse of `thread_key`.
pub fn parse_thread_key(key: &str) -> Option<Thread> {
    match key.strip_prefix("group:") {
        Some(k) => Some(Thread::Group(hex::decode(k).ok()?.try_into().ok()?)),
        None => ServiceId::parse_from_service_id_string(key).map(Thread::Contact),
    }
}

pub fn thread_key(t: &Thread) -> String {
    match t {
        Thread::Contact(id) => id.service_id_string(),
        Thread::Group(key) => format!("group:{}", hex::encode(key)),
    }
}

impl AppState {
    /// A missing or corrupt file starts fresh: read markers are cheap to lose.
    pub fn load(path: &Path) -> Self {
        let mut s: AppState = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        s.path = path.to_owned();
        s
    }

    pub fn last_read(&self, t: &Thread) -> u64 {
        self.last_read.get(&thread_key(t)).copied().unwrap_or(0)
    }

    /// Only moves forward; returns true (and saves) if it changed.
    pub fn mark_read(&mut self, t: &Thread, ts: u64) -> bool {
        let e = self.last_read.entry(thread_key(t)).or_insert(0);
        if ts <= *e {
            return false;
        }
        *e = ts;
        self.save();
        true
    }

    /// Replaces the storage-service snapshot and saves it.
    pub fn set_storage(&mut self, listed: HashSet<String>, profile_names: HashMap<String, String>) {
        self.listed = listed;
        self.profile_names = profile_names;
        self.save();
    }

    /// Starts a countdown unless one is already running; returns true (and saves) if new.
    pub fn start_expiry(&mut self, t: &Thread, author: Uuid, ts: u64, expires_at: u64) -> bool {
        let thread = thread_key(t);
        if self.expiring.iter().any(|e| e.thread == thread && e.author == author && e.ts == ts) {
            return false;
        }
        self.expiring.push(Expiry { thread, author, ts, expires_at });
        self.save();
        true
    }

    pub fn expiries(&self, t: &Thread) -> impl Iterator<Item = &Expiry> {
        let key = thread_key(t);
        self.expiring.iter().filter(move |e| e.thread == key)
    }

    /// The earliest pending expiry, for the backend's sleep.
    pub fn next_expiry(&self) -> Option<u64> {
        self.expiring.iter().map(|e| e.expires_at).min()
    }

    /// Removes (and saves) every countdown that ran out by `now_ms`.
    pub fn take_due(&mut self, now_ms: u64) -> Vec<Expiry> {
        let (due, keep) = std::mem::take(&mut self.expiring).into_iter().partition(|e| e.expires_at <= now_ms);
        self.expiring = keep;
        if !due.is_empty() {
            self.save();
        }
        due
    }

    pub fn sent_sticker(&mut self, sticker: (Vec<u8>, u32)) {
        push_recent(&mut self.recent_stickers, sticker);
        self.save();
    }

    fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        let ok = serde_json::to_vec(self)
            .ok()
            .is_some_and(|b| std::fs::write(&tmp, b).is_ok() && std::fs::rename(&tmp, &self.path).is_ok());
        if !ok {
            tracing::warn!(path = %self.path.display(), "failed to save app state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::protocol::ServiceId;

    #[test]
    fn read_markers_persist_and_only_advance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let alice = Thread::Contact(ServiceId::Aci(crate::timeline::tests::uuid(2).into()));
        let group = Thread::Group([9; 32]);

        let mut s = AppState::load(&path);
        assert_eq!(s.last_read(&alice), 0);
        assert!(s.mark_read(&alice, 10));
        assert!(!s.mark_read(&alice, 5), "never moves backwards");
        assert!(s.mark_read(&group, 7));

        let s2 = AppState::load(&path);
        assert_eq!((s2.last_read(&alice), s2.last_read(&group)), (10, 7));
    }

    #[test]
    fn storage_snapshot_persists_and_old_files_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"last_read":{"group:01":5}}"#).unwrap();
        let mut s = AppState::load(&path);
        assert!(s.listed.is_empty(), "files from before this field load fine");
        s.set_storage(HashSet::from(["x".to_string()]), HashMap::from([("u".to_string(), "Ana".to_string())]));
        let s2 = AppState::load(&path);
        assert!(s2.listed.contains("x"));
        assert_eq!(s2.profile_names["u"], "Ana");
    }

    #[test]
    fn recent_stickers_persist_newest_first_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"last_read":{}}"#).unwrap();
        let mut s = AppState::load(&path);
        assert!(s.recent_stickers.is_empty(), "older files load without the field");
        s.sent_sticker((vec![1], 1));
        s.sent_sticker((vec![1], 2));
        s.sent_sticker((vec![1], 1));
        assert_eq!(AppState::load(&path).recent_stickers, [(vec![1], 1), (vec![1], 2)]);
        for i in 0..30 {
            s.sent_sticker((vec![2], i));
        }
        assert_eq!(s.recent_stickers.len(), RECENT_STICKERS);
        assert_eq!(s.recent_stickers[0], (vec![2], 29));
    }

    #[test]
    fn corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(AppState::load(&path).last_read(&Thread::Group([1; 32])), 0);
    }

    #[test]
    fn expiry_math() {
        assert_eq!(expires_at(1_000, 30), 31_000);
        assert_eq!(remaining_s(31_000, 1_000), 30);
        assert_eq!(remaining_s(31_000, 30_001), 1, "rounds up");
        assert_eq!(remaining_s(31_000, 40_000), 0);
    }

    #[test]
    fn countdowns_survive_restart_and_fire_when_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let a = crate::timeline::tests::uuid(2);
        let alice = Thread::Contact(ServiceId::Aci(a.into()));
        let group = Thread::Group([9; 32]);

        let mut s = AppState::load(&path);
        assert_eq!(s.next_expiry(), None);
        assert!(s.start_expiry(&alice, a, 10, 5_000));
        assert!(!s.start_expiry(&alice, a, 10, 9_000), "a running countdown never restarts");
        assert!(s.start_expiry(&group, a, 10, 2_000));

        let mut s = AppState::load(&path);
        assert_eq!(s.next_expiry(), Some(2_000), "restart keeps the countdowns");
        assert_eq!(s.expiries(&alice).count(), 1);
        assert!(s.take_due(1_999).is_empty());
        let due = s.take_due(2_000);
        assert_eq!(due.iter().map(|e| parse_thread_key(&e.thread)).collect::<Vec<_>>(), [Some(group)]);
        assert_eq!(AppState::load(&path).next_expiry(), Some(5_000), "fired ones are forgotten on disk");
    }

    #[test]
    fn thread_keys_round_trip() {
        let c = Thread::Contact(ServiceId::Aci(crate::timeline::tests::uuid(1).into()));
        for t in [c, Thread::Group([7; 32])] {
            assert_eq!(parse_thread_key(&thread_key(&t)), Some(t));
        }
        assert_eq!(parse_thread_key("group:zz"), None);
    }

    #[test]
    fn thread_keys_are_distinct() {
        let c = Thread::Contact(ServiceId::Aci(crate::timeline::tests::uuid(1).into()));
        assert_ne!(thread_key(&c), thread_key(&Thread::Group([1; 32])));
    }
}
