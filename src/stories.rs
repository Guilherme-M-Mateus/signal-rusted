//! Stories: the received ones (kept for Signal's 24 hours), the "Stories" list order, the
//! full-window viewer's state machine and story replies.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use presage::libsignal_service::prelude::{ProtobufMessage, Uuid};
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::data_message::{Reaction, StoryContext};
use presage::proto::story_message::Attachment;
use presage::proto::{AttachmentPointer, DataMessage, StoryMessage, TextAttachment};
use presage::store::Thread;
use serde::{Deserialize, Serialize};

/// Signal shows a story for a day after it was sent.
pub const STORY_TTL_MS: u64 = 24 * 3600 * 1000;
/// How long an image or text story stays up before the viewer moves on (Signal: 5s).
pub const STORY_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Story {
    pub author: Uuid,
    /// Sent timestamp; with `author`, what replies and receipts point at.
    pub ts: u64,
    /// Master key of the group it was posted to; `None` for the author's own story lists.
    pub group: Option<[u8; 32]>,
    #[serde(default)]
    pub viewed: bool,
    #[serde(with = "proto_hex")]
    pub msg: StoryMessage,
}

/// The protobuf, hex-encoded: prost types aren't serde types.
mod proto_hex {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S: serde::Serializer>(m: &StoryMessage, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(m.encode_to_vec()))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<StoryMessage, D::Error> {
        let b = hex::decode(String::deserialize(d)?).map_err(D::Error::custom)?;
        StoryMessage::decode(b.as_slice()).map_err(D::Error::custom)
    }
}

impl Story {
    pub fn new(author: Uuid, ts: u64, msg: StoryMessage) -> Self {
        let group = msg.group.as_ref().and_then(|g| g.master_key.as_deref()).and_then(|k| k.try_into().ok());
        Story { author, ts, group, viewed: false, msg }
    }

    /// The list entry it belongs to: the group, or the author.
    pub fn owner(&self) -> Thread {
        match self.group {
            Some(k) => Thread::Group(k),
            None => Thread::Contact(ServiceId::Aci(self.author.into())),
        }
    }

    pub fn expires_at(&self) -> u64 {
        self.ts + STORY_TTL_MS
    }

    pub fn file(&self) -> Option<&AttachmentPointer> {
        match &self.msg.attachment {
            Some(Attachment::FileAttachment(p)) => Some(p),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&TextAttachment> {
        match &self.msg.attachment {
            Some(Attachment::TextAttachment(t)) => Some(t),
            _ => None,
        }
    }

    pub fn is_video(&self) -> bool {
        self.file().and_then(|p| p.content_type.as_deref()).is_some_and(|c| c.starts_with("video/"))
    }

    pub fn allows_replies(&self) -> bool {
        self.msg.allows_replies.unwrap_or(false)
    }
}

/// Received stories, saved to a JSON file in the data dir.
#[derive(Default, Serialize, Deserialize)]
pub struct StoryStore {
    pub stories: Vec<Story>,
    #[serde(skip)]
    path: PathBuf,
}

impl StoryStore {
    /// A missing or corrupt file starts empty: stories are gone within a day anyway.
    pub fn load(path: &Path) -> Self {
        let mut s: StoryStore =
            std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        s.path = path.to_owned();
        s
    }

    fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        let ok = serde_json::to_vec(self)
            .ok()
            .is_some_and(|b| std::fs::write(&tmp, b).is_ok() && std::fs::rename(&tmp, &self.path).is_ok());
        if !ok {
            tracing::warn!(path = %self.path.display(), "failed to save stories");
        }
    }

    /// Adds (and saves) a story unless it's already known or already expired.
    pub fn add(&mut self, s: Story, now_ms: u64) -> bool {
        if s.expires_at() <= now_ms || self.get(s.author, s.ts).is_some() {
            return false;
        }
        self.stories.push(s);
        self.save();
        true
    }

    pub fn get(&self, author: Uuid, ts: u64) -> Option<&Story> {
        self.stories.iter().find(|s| s.author == author && s.ts == ts)
    }

    /// Returns true (and saves) the first time.
    pub fn mark_viewed(&mut self, author: Uuid, ts: u64) -> bool {
        match self.stories.iter_mut().find(|s| s.author == author && s.ts == ts && !s.viewed) {
            Some(s) => s.viewed = true,
            None => return false,
        }
        self.save();
        true
    }

    /// When the next story expires, for the backend's sleep.
    pub fn next_expiry(&self) -> Option<u64> {
        self.stories.iter().map(Story::expires_at).min()
    }

    /// Removes (and saves) the expired stories, returning them so their media can go too.
    pub fn take_expired(&mut self, now_ms: u64) -> Vec<Story> {
        let (gone, keep) = std::mem::take(&mut self.stories).into_iter().partition(|s| s.expires_at() <= now_ms);
        self.stories = keep;
        if !gone.is_empty() {
            self.save();
        }
        gone
    }
}

/// One row of the Stories list: a person's or group's active stories, oldest first.
pub struct Entry<'a> {
    pub owner: Thread,
    pub stories: Vec<&'a Story>,
}

impl Entry<'_> {
    pub fn unviewed(&self) -> bool {
        self.stories.iter().any(|s| !s.viewed)
    }

    pub fn newest(&self) -> u64 {
        self.stories.last().map_or(0, |s| s.ts)
    }
}

fn of<'a>(stories: &'a [Story], owner: &Thread, now_ms: u64) -> Vec<&'a Story> {
    let mut v: Vec<&Story> = stories.iter().filter(|s| s.owner() == *owner && s.expires_at() > now_ms).collect();
    v.sort_by_key(|s| s.ts);
    v
}

/// Signal's order: owners with unviewed stories first, then by newest story.
pub fn list(stories: &[Story], now_ms: u64) -> Vec<Entry<'_>> {
    let mut owners: Vec<Thread> = Vec::new();
    for s in stories {
        if !owners.contains(&s.owner()) {
            owners.push(s.owner());
        }
    }
    let mut out: Vec<Entry> = owners
        .into_iter()
        .map(|owner| Entry { stories: of(stories, &owner, now_ms), owner })
        .filter(|e| !e.stories.is_empty())
        .collect();
    out.sort_by_key(|e| (!e.unviewed(), std::cmp::Reverse(e.newest())));
    out
}

/// The full-window viewer: which story is up, since when, and the reply being typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    /// Owners in list order when it opened, so finishing one doesn't reshuffle what comes next.
    pub queue: Vec<Thread>,
    pub owner: usize,
    pub index: usize,
    pub started: Instant,
    pub reply: String,
}

fn first_unviewed(v: &[&Story]) -> usize {
    v.iter().position(|s| !s.viewed).unwrap_or(0)
}

impl Viewer {
    /// Opens at `owner`'s first unviewed story, like Signal.
    pub fn open(stories: &[Story], owner: &Thread, now: Instant, now_ms: u64) -> Option<Viewer> {
        let queue: Vec<Thread> = list(stories, now_ms).into_iter().map(|e| e.owner).collect();
        let at = queue.iter().position(|o| o == owner)?;
        let index = first_unviewed(&of(stories, owner, now_ms));
        Some(Viewer { queue, owner: at, index, started: now, reply: String::new() })
    }

    pub fn current<'a>(&self, stories: &'a [Story], now_ms: u64) -> Option<&'a Story> {
        of(stories, self.queue.get(self.owner)?, now_ms).get(self.index).copied()
    }

    /// (position, count) within the current owner's stories, for the progress bar.
    pub fn progress(&self, stories: &[Story], now_ms: u64) -> (usize, usize) {
        (self.index, self.queue.get(self.owner).map_or(0, |o| of(stories, o, now_ms).len()))
    }

    /// Moves `delta` stories, across owners; false past the last one (the viewer closes).
    /// Back from the very first story stays on it.
    pub fn step(&mut self, stories: &[Story], delta: isize, now: Instant, now_ms: u64) -> bool {
        let at = |o: usize| self.queue.get(o).map_or_else(Vec::new, |t| of(stories, t, now_ms));
        let (mut o, mut i) = (self.owner, self.index as isize + delta);
        loop {
            if i < 0 {
                if o == 0 {
                    i = 0;
                    if !at(0).is_empty() {
                        break;
                    }
                } else {
                    o -= 1;
                    i = at(o).len() as isize - 1;
                }
            } else if i as usize >= at(o).len() {
                o += 1;
                if o >= self.queue.len() {
                    return false;
                }
                i = first_unviewed(&at(o)) as isize;
            } else {
                break;
            }
        }
        (self.owner, self.index, self.started) = (o, i as usize, now);
        self.reply.clear();
        true
    }

    /// Time to auto-advance: shown long enough and no reply being typed.
    pub fn due(&self, now: Instant) -> bool {
        self.reply.is_empty() && now.duration_since(self.started) >= STORY_DURATION
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StoryReply {
    Text(String),
    Reaction(String),
}

/// The 1:1 message answering `author`'s story sent at `ts`.
pub fn reply_message(author: Uuid, ts: u64, reply: StoryReply) -> DataMessage {
    let story_context =
        Some(StoryContext { author_aci: Some(author.to_string()), sent_timestamp: Some(ts), ..Default::default() });
    match reply {
        StoryReply::Text(body) => DataMessage { body: Some(body), story_context, ..Default::default() },
        StoryReply::Reaction(emoji) => DataMessage {
            reaction: Some(Reaction {
                emoji: Some(emoji),
                remove: Some(false),
                target_author_aci: Some(author.to_string()),
                target_sent_timestamp: Some(ts),
                ..Default::default()
            }),
            story_context,
            ..Default::default()
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::timeline::tests::uuid;
    use presage::proto::GroupContextV2;

    pub fn text_story(author: u8, ts: u64, body: &str) -> Story {
        let msg = StoryMessage {
            attachment: Some(Attachment::TextAttachment(TextAttachment {
                text: Some(body.into()),
                text_foreground_color: Some(0xffffffff),
                background: Some(presage::proto::text_attachment::Background::Color(0xff2c6bed)),
                ..Default::default()
            })),
            allows_replies: Some(true),
            ..Default::default()
        };
        Story::new(uuid(author), ts, msg)
    }

    pub fn group_story(author: u8, ts: u64, key: [u8; 32]) -> Story {
        let mut s = text_story(author, ts, "group story");
        s.msg.group = Some(GroupContextV2 { master_key: Some(key.to_vec()), ..Default::default() });
        Story::new(s.author, ts, s.msg)
    }

    const DAY: u64 = STORY_TTL_MS;

    #[test]
    fn store_dedupes_persists_views_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stories.json");
        let mut s = StoryStore::load(&path);
        assert!(s.add(text_story(2, 1000, "a"), 2000));
        assert!(!s.add(text_story(2, 1000, "a again"), 2000), "same author + ts");
        assert!(!s.add(text_story(2, 5, "old"), 5 + DAY), "already expired");
        assert!(s.add(group_story(3, 3000, [7; 32]), 3000));
        assert!(s.mark_viewed(uuid(2), 1000));
        assert!(!s.mark_viewed(uuid(2), 1000), "only once");

        let mut s = StoryStore::load(&path);
        assert_eq!(s.stories.len(), 2);
        assert!(s.get(uuid(2), 1000).unwrap().viewed, "viewed state persisted");
        assert_eq!(s.get(uuid(3), 3000).unwrap().group, Some([7; 32]));
        assert_eq!(s.get(uuid(3), 3000).unwrap().text().unwrap().text.as_deref(), Some("group story"));
        assert_eq!(s.next_expiry(), Some(1000 + DAY));

        assert!(s.take_expired(1000 + DAY - 1).is_empty());
        let gone = s.take_expired(1000 + DAY);
        assert_eq!(gone.iter().map(|s| s.ts).collect::<Vec<_>>(), [1000]);
        assert_eq!(StoryStore::load(&path).stories.len(), 1, "expiry persisted");
    }

    #[test]
    fn list_puts_unviewed_first_then_newest_and_groups_by_owner() {
        let mut stories = vec![
            text_story(2, 100, "alice 1"),
            text_story(3, 300, "bob"),
            text_story(2, 200, "alice 2"),
            group_story(4, 250, [7; 32]),
            text_story(5, 400, "carol"),
        ];
        stories[1].viewed = true; // bob: all viewed, though newest after carol
        let l = list(&stories, 500);
        let owners: Vec<Thread> = l.iter().map(|e| e.owner.clone()).collect();
        let contact = |n| Thread::Contact(ServiceId::Aci(uuid(n).into()));
        assert_eq!(owners, [contact(5), Thread::Group([7; 32]), contact(2), contact(3)]);
        assert_eq!(l[2].stories.iter().map(|s| s.ts).collect::<Vec<_>>(), [100, 200], "oldest first");
        assert!(list(&stories, 400 + DAY).is_empty(), "expired ones are hidden");
    }

    #[test]
    fn viewer_starts_unviewed_advances_across_owners_and_closes() {
        let mut stories = vec![text_story(2, 100, "a1"), text_story(2, 200, "a2"), text_story(3, 50, "b1")];
        stories[0].viewed = true;
        let t0 = Instant::now();
        let alice = Thread::Contact(ServiceId::Aci(uuid(2).into()));
        let mut v = Viewer::open(&stories, &alice, t0, 1000).unwrap();
        assert_eq!(v.current(&stories, 1000).unwrap().ts, 200, "first unviewed");
        assert_eq!(v.progress(&stories, 1000), (1, 2));

        assert!(v.step(&stories, -1, t0, 1000));
        assert_eq!(v.current(&stories, 1000).unwrap().ts, 100);
        assert!(v.step(&stories, -1, t0, 1000), "back from the very first stays");
        assert_eq!(v.current(&stories, 1000).unwrap().ts, 100);

        // Auto-advance through the clock seam, paused while typing a reply.
        let later = t0 + STORY_DURATION;
        assert!(!v.due(t0 + Duration::from_secs(4)));
        v.reply = "typing".into();
        assert!(!v.due(later));
        v.reply.clear();
        assert!(v.due(later));

        assert!(v.step(&stories, 1, later, 1000));
        assert_eq!((v.current(&stories, 1000).unwrap().ts, v.started), (200, later), "timer restarts");
        assert!(v.step(&stories, 1, later, 1000));
        assert_eq!(v.current(&stories, 1000).unwrap().author, uuid(3), "on to the next person");
        assert!(v.step(&stories, -1, later, 1000));
        assert_eq!(v.current(&stories, 1000).unwrap().ts, 200, "back into the previous person's last");
        assert!(v.step(&stories, 1, later, 1000));
        assert!(!v.step(&stories, 1, later, 1000), "past the last story closes");
        assert!(Viewer::open(&stories, &Thread::Group([1; 32]), t0, 1000).is_none(), "no stories");
    }

    #[test]
    fn replies_carry_the_story_context() {
        let ctx = Some(StoryContext { author_aci: Some(uuid(2).to_string()), sent_timestamp: Some(100), ..Default::default() });
        let dm = reply_message(uuid(2), 100, StoryReply::Text("nice".into()));
        assert_eq!((dm.body.as_deref(), &dm.story_context, dm.reaction.is_none()), (Some("nice"), &ctx, true));
        let dm = reply_message(uuid(2), 100, StoryReply::Reaction("❤️".into()));
        let r = dm.reaction.unwrap();
        assert_eq!((r.emoji.as_deref(), r.remove, r.target_sent_timestamp), (Some("❤️"), Some(false), Some(100)));
        assert_eq!(r.target_author_aci, Some(uuid(2).to_string()));
        assert_eq!((dm.body, dm.story_context), (None, ctx));
    }
}
