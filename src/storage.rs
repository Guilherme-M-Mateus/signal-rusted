//! Storage service: the account's synced chat list (contacts you've accepted, groups),
//! which a newly linked device needs before any message arrives.

use std::collections::{HashMap, HashSet};

use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::proto::StorageRecord;
use presage::libsignal_service::proto::storage_record::Record;
use presage::store::Thread;

use crate::state::thread_key;

#[derive(Default, Debug, PartialEq)]
pub struct Plan {
    /// Thread keys to show in the conversation list.
    pub listed: HashSet<String>,
    /// ACI -> best display name from the record (nickname, system name, profile name).
    pub names: HashMap<String, String>,
    /// Group master keys to fetch from the groups server.
    pub groups: Vec<[u8; 32]>,
    pub profile_keys: Vec<(Uuid, [u8; 32])>,
}

fn join(given: &str, family: &str) -> String {
    format!("{given} {family}").trim().to_owned()
}

pub fn plan(records: &[StorageRecord]) -> Plan {
    let mut p = Plan::default();
    for r in records {
        match &r.record {
            Some(Record::Contact(c)) => {
                let aci = Uuid::from_slice(&c.aci_binary).ok().or_else(|| Uuid::parse_str(&c.aci).ok());
                let Some(aci) = aci else { continue }; // PNI-only/e164-only entries can't be messaged yet
                let name = [
                    c.nickname.as_ref().map(|n| join(&n.given, &n.family)).unwrap_or_default(),
                    c.system_nickname.clone(),
                    join(&c.system_given_name, &c.system_family_name),
                    join(&c.given_name, &c.family_name),
                ]
                .into_iter()
                .find(|n| !n.is_empty());
                if let Some(name) = name {
                    p.names.insert(aci.to_string(), name);
                }
                if let Ok(k) = <[u8; 32]>::try_from(c.profile_key.as_slice()) {
                    p.profile_keys.push((aci, k));
                }
                if c.whitelisted && !c.blocked && !c.hidden && !c.archived {
                    p.listed.insert(thread_key(&Thread::Contact(ServiceId::Aci(aci.into()))));
                }
            }
            Some(Record::GroupV2(g)) if !g.blocked && !g.archived => {
                if let Ok(k) = <[u8; 32]>::try_from(g.master_key.as_slice()) {
                    p.groups.push(k);
                    p.listed.insert(thread_key(&Thread::Group(k)));
                }
            }
            _ => {}
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{ContactRecord, GroupV2Record, contact_record};

    fn contact(n: u8, f: impl FnOnce(&mut ContactRecord)) -> StorageRecord {
        let mut c = ContactRecord { aci_binary: vec![n; 16], profile_key: vec![9; 32], whitelisted: true, ..Default::default() };
        f(&mut c);
        StorageRecord { record: Some(Record::Contact(c)) }
    }

    fn key(n: u8) -> String {
        thread_key(&Thread::Contact(ServiceId::Aci(Uuid::from_bytes([n; 16]).into())))
    }

    #[test]
    fn accepted_contacts_are_listed_with_best_name() {
        let p = plan(&[
            contact(1, |c| {
                c.given_name = "Profile".into();
                c.system_given_name = "Ana".into();
                c.system_family_name = "Silva".into();
            }),
            contact(2, |c| {
                c.given_name = "Bo".into();
                c.nickname = Some(contact_record::Name { given: "Bobby".into(), family: String::new() });
            }),
            contact(3, |c| c.given_name = "Profile Only".into()),
        ]);
        assert_eq!(p.names[&Uuid::from_bytes([1; 16]).to_string()], "Ana Silva", "system name beats profile");
        assert_eq!(p.names[&Uuid::from_bytes([2; 16]).to_string()], "Bobby", "nickname wins");
        assert_eq!(p.names[&Uuid::from_bytes([3; 16]).to_string()], "Profile Only");
        assert_eq!(p.listed, HashSet::from([key(1), key(2), key(3)]));
        assert_eq!(p.profile_keys.len(), 3);
    }

    #[test]
    fn blocked_hidden_archived_and_unaccepted_are_not_listed() {
        let p = plan(&[
            contact(1, |c| c.blocked = true),
            contact(2, |c| c.hidden = true),
            contact(3, |c| c.archived = true),
            contact(4, |c| c.whitelisted = false),
        ]);
        assert!(p.listed.is_empty());
        assert_eq!(p.profile_keys.len(), 4, "profile keys still help sealed sender");
    }

    #[test]
    fn string_aci_fallback_and_invalid_entries() {
        let p = plan(&[
            contact(0, |c| {
                c.aci_binary.clear();
                c.aci = Uuid::from_bytes([5; 16]).to_string();
            }),
            contact(0, |c| c.aci_binary = vec![1, 2, 3]),
            contact(6, |c| c.profile_key = vec![1]),
            StorageRecord { record: None },
        ]);
        assert!(p.listed.contains(&key(5)));
        assert!(p.listed.contains(&key(6)));
        assert_eq!(p.listed.len(), 2, "malformed ACI skipped");
        assert_eq!(p.profile_keys.len(), 1, "short profile key skipped");
    }

    #[test]
    fn groups_are_fetched_unless_blocked_or_archived() {
        let g = |n: u8, f: fn(&mut GroupV2Record)| {
            let mut r = GroupV2Record { master_key: vec![n; 32], ..Default::default() };
            f(&mut r);
            StorageRecord { record: Some(Record::GroupV2(r)) }
        };
        let p = plan(&[g(1, |_| {}), g(2, |r| r.blocked = true), g(3, |r| r.archived = true), g(4, |r| r.master_key = vec![1])]);
        assert_eq!(p.groups, vec![[1; 32]]);
        assert!(p.listed.contains(&thread_key(&Thread::Group([1; 32]))));
    }
}
