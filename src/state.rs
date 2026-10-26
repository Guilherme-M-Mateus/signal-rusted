//! Small app-level state presage doesn't track: read markers and the storage-service chat list.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    #[serde(skip)]
    path: PathBuf,
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
    fn corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(AppState::load(&path).last_read(&Thread::Group([1; 32])), 0);
    }

    #[test]
    fn thread_keys_are_distinct() {
        let c = Thread::Contact(ServiceId::Aci(crate::timeline::tests::uuid(1).into()));
        assert_ne!(thread_key(&c), thread_key(&Thread::Group([1; 32])));
    }
}
