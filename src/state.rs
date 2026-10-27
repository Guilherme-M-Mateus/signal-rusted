//! Small app-level state presage doesn't track: read markers, the storage-service chat list
//! and disappearing-message countdowns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::store::Thread;
use serde::{Deserialize, Serialize};

use crate::timeline::{Delivery, Timeline};

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
    /// Per-chat archive/mute/unread/block flags, as currently in effect here.
    #[serde(default)]
    pub flags: HashMap<String, ChatFlags>,
    /// The flags the storage service had at the last sync: only changes made since on another
    /// device override ours (we don't write ours back, so a restart mustn't undo them).
    #[serde(default)]
    synced_flags: HashMap<String, ChatFlags>,
    /// Pinned chats in order (at most `MAX_PINNED`).
    #[serde(default)]
    pub pinned: Vec<String>,
    #[serde(default)]
    synced_pinned: Vec<String>,
    /// Message requests accepted on this device.
    #[serde(default)]
    pub accepted: HashSet<String>,
    /// Recently sent stickers, newest first: (pack id, sticker id).
    #[serde(default)]
    pub recent_stickers: Vec<(Vec<u8>, u32)>,
    /// Best receipt per own group message: thread key -> sent ts -> level. presage files
    /// receipts under the sender's 1:1 chat, so group timelines would lose them on restart.
    // shortcut: entries outlive deleted messages; prune if state.json ever gets large.
    #[serde(default)]
    receipts: HashMap<String, HashMap<u64, Delivery>>,
    #[serde(skip)]
    path: PathBuf,
}

pub const MAX_PINNED: usize = 4;

#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct ChatFlags {
    pub archived: bool,
    /// Ms timestamp; `u64::MAX` is "until I turn it back on", as in Signal's storage records.
    pub muted_until: u64,
    pub marked_unread: bool,
    pub blocked: bool,
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

    /// Replaces the storage-service snapshot and saves it. Flags and pins take the storage
    /// value only where it changed since the last sync, so local-only changes survive.
    pub fn set_storage(
        &mut self,
        listed: HashSet<String>,
        profile_names: HashMap<String, String>,
        flags: HashMap<String, ChatFlags>,
        pinned: Vec<String>,
    ) {
        self.listed = listed;
        self.profile_names = profile_names;
        let keys: HashSet<String> = flags.keys().chain(self.synced_flags.keys()).cloned().collect();
        for k in keys {
            let new = flags.get(&k).cloned().unwrap_or_default();
            if self.synced_flags.get(&k).cloned().unwrap_or_default() != new {
                self.flags.insert(k, new);
            }
        }
        self.synced_flags = flags;
        if pinned != self.synced_pinned {
            self.pinned = pinned.iter().take(MAX_PINNED).cloned().collect();
            self.synced_pinned = pinned;
        }
        self.save();
    }

    pub fn flags(&self, t: &Thread) -> ChatFlags {
        self.flags.get(&thread_key(t)).cloned().unwrap_or_default()
    }

    /// Changes one chat's flags and saves.
    pub fn update_flags(&mut self, t: &Thread, f: impl FnOnce(&mut ChatFlags)) {
        f(self.flags.entry(thread_key(t)).or_default());
        self.save();
    }

    pub fn is_muted(&self, t: &Thread, now_ms: u64) -> bool {
        self.flags(t).muted_until > now_ms
    }

    pub fn is_pinned(&self, t: &Thread) -> bool {
        self.pinned.contains(&thread_key(t))
    }

    /// Pins (at the end, like Signal) or unpins; false if already at `MAX_PINNED`.
    pub fn set_pinned(&mut self, t: &Thread, pin: bool) -> bool {
        let k = thread_key(t);
        if pin && !self.pinned.contains(&k) {
            if self.pinned.len() >= MAX_PINNED {
                return false;
            }
            self.pinned.push(k);
        } else if !pin {
            self.pinned.retain(|p| *p != k);
        }
        self.save();
        true
    }

    /// A 1:1 chat from someone we never accepted: not whitelisted in the storage service, not
    /// from the address book (`in_contacts`), not accepted here. Groups and Note to Self never are.
    pub fn is_request(&self, t: &Thread, me: Uuid, in_contacts: bool) -> bool {
        let k = thread_key(t);
        matches!(t, Thread::Contact(id) if id.raw_uuid() != me)
            && !in_contacts
            && !self.listed.contains(&k)
            && !self.accepted.contains(&k)
    }

    pub fn accept(&mut self, t: &Thread) {
        self.accepted.insert(thread_key(t));
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

    /// Remembers receipts for messages in `t`, upgrade-only; saves if anything changed.
    pub fn record_receipts(&mut self, t: &Thread, timestamps: &[u64], level: Delivery) -> bool {
        let known = self.receipts.entry(thread_key(t)).or_default();
        let mut changed = false;
        for ts in timestamps {
            let e = known.entry(*ts).or_insert(Delivery::Sent);
            if level > *e {
                *e = level;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
        changed
    }

    /// Applies the remembered receipts to a freshly loaded timeline of `t`.
    pub fn apply_receipts(&self, t: &Thread, timeline: &mut Timeline) {
        for (ts, level) in self.receipts.get(&thread_key(t)).into_iter().flatten() {
            timeline.receipt(&[*ts], *level);
        }
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
    fn group_receipts_persist_upgrade_only_and_apply_on_load() {
        use crate::timeline::tests::{content, text, uuid};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let group = Thread::Group([9; 32]);
        let mut s = AppState::load(&path);
        assert!(s.record_receipts(&group, &[10, 20], Delivery::Delivered));
        assert!(s.record_receipts(&group, &[10], Delivery::Read));
        assert!(!s.record_receipts(&group, &[10, 20], Delivery::Delivered), "never downgrades");

        let mut t = Timeline::new(uuid(1));
        for ts in [10, 20, 30] {
            t.apply(&content(uuid(1), ts, text(ts, "mine")));
        }
        t.apply(&content(uuid(2), 20, text(20, "theirs, same ts")));
        AppState::load(&path).apply_receipts(&group, &mut t);
        let got: Vec<_> = t.messages.iter().map(|m| (m.id, m.outgoing, m.delivery)).collect();
        assert_eq!(
            got,
            [(10, true, Delivery::Read), (20, true, Delivery::Delivered), (20, false, Delivery::Delivered), (30, true, Delivery::Sent)]
        );
        let mut other = Timeline::new(uuid(1));
        other.apply(&content(uuid(1), 10, text(10, "other chat")));
        AppState::load(&path).apply_receipts(&Thread::Group([8; 32]), &mut other);
        assert_eq!(other.messages[0].delivery, Delivery::Sent, "per chat");
    }

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
        s.set_storage(HashSet::from(["x".to_string()]), HashMap::from([("u".to_string(), "Ana".to_string())]), HashMap::new(), Vec::new());
        let s2 = AppState::load(&path);
        assert!(s2.listed.contains("x"));
        assert_eq!(s2.profile_names["u"], "Ana");
    }

    fn contact(n: u8) -> Thread {
        Thread::Contact(ServiceId::Aci(crate::timeline::tests::uuid(n).into()))
    }

    #[test]
    fn requests_are_unaccepted_one_to_one_chats() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AppState::load(&dir.path().join("state.json"));
        let me = crate::timeline::tests::uuid(1);
        s.listed.insert(thread_key(&contact(2)));
        assert!(s.is_request(&contact(3), me, false), "stranger");
        assert!(!s.is_request(&contact(3), me, true), "address book contact");
        assert!(!s.is_request(&contact(2), me, false), "whitelisted in storage");
        assert!(!s.is_request(&contact(1), me, false), "Note to Self");
        assert!(!s.is_request(&Thread::Group([1; 32]), me, false), "groups");
        s.accept(&contact(3));
        assert!(!AppState::load(&dir.path().join("state.json")).is_request(&contact(3), me, false), "accepting persists");
    }

    #[test]
    fn pins_keep_order_and_stop_at_four() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AppState::load(&dir.path().join("state.json"));
        for n in 2..=5 {
            assert!(s.set_pinned(&contact(n), true));
        }
        assert!(!s.set_pinned(&contact(6), true), "fifth pin refused");
        assert!(s.set_pinned(&contact(3), false));
        assert!(s.set_pinned(&contact(6), true));
        let keys: Vec<String> = [2, 4, 5, 6].map(|n| thread_key(&contact(n))).into();
        assert_eq!(AppState::load(&dir.path().join("state.json")).pinned, keys);
    }

    #[test]
    fn mute_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AppState::load(&dir.path().join("state.json"));
        assert!(!s.is_muted(&contact(2), 0));
        s.update_flags(&contact(2), |f| f.muted_until = 100);
        assert!(s.is_muted(&contact(2), 99) && !s.is_muted(&contact(2), 100));
    }

    #[test]
    fn storage_flags_only_override_what_changed_remotely() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AppState::load(&dir.path().join("state.json"));
        let (a, b) = (contact(2), contact(3));
        let archived = ChatFlags { archived: true, ..Default::default() };
        let sync = |s: &mut AppState, flags: Vec<(&Thread, ChatFlags)>, pinned: Vec<&Thread>| {
            let flags = flags.into_iter().map(|(t, f)| (thread_key(t), f)).collect();
            s.set_storage(HashSet::new(), HashMap::new(), flags, pinned.into_iter().map(thread_key).collect());
        };
        sync(&mut s, vec![(&a, archived.clone())], vec![&b]);
        assert!(s.flags(&a).archived && s.is_pinned(&b), "seeded");

        s.update_flags(&a, |f| f.archived = false); // local-only unarchive
        s.set_pinned(&a, true);
        sync(&mut s, vec![(&a, archived.clone())], vec![&b]);
        assert!(!s.flags(&a).archived && s.is_pinned(&a), "unchanged storage keeps local changes");

        sync(&mut s, vec![(&a, archived.clone()), (&b, ChatFlags { muted_until: u64::MAX, ..Default::default() })], vec![]);
        assert!(s.is_muted(&b, 0) && !s.is_pinned(&b) && !s.is_pinned(&a), "remote changes win");
        sync(&mut s, vec![], vec![]);
        assert!(s.flags(&a) == ChatFlags::default(), "unarchived remotely");
        assert!(AppState::load(&dir.path().join("state.json")).flags.values().all(|f| *f == ChatFlags::default()));
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
