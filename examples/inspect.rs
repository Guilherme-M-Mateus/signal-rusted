//! Read-only summary of the local store: per-thread counts, no message content.
//! `cargo run --release --example inspect [data_dir]`
use presage::model::identity::OnNewIdentity;
use presage::store::{ContentsStore, StateStore, Thread};
use presage_store_sqlite::SqliteStore;
use signal_rs::timeline::{Kind, Timeline};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).map(std::path::PathBuf::from).unwrap_or_else(|| {
        directories::ProjectDirs::from("org", "signal-rs", "signal-rs").unwrap().data_dir().to_owned()
    });
    let pass = signal_rs::dbkey::db_passphrase(&dir, &signal_rs::dbkey::OsKeyring::new(&dir))?;
    let store = SqliteStore::open_with_passphrase(&dir.join("signal.db3").to_string_lossy(), Some(pass.trim()), OnNewIdentity::Trust).await?;
    let me = presage::libsignal_service::protocol::ServiceId::from(store.load_registration_data().await?.unwrap().service_ids.aci()).raw_uuid();
    let mut threads: Vec<(Thread, String)> = store.groups().await?.flatten().map(|(k, g)| (Thread::Group(k), g.title)).collect();
    for c in store.contacts().await?.flatten() {
        threads.push((Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(c.uuid.into())), c.name));
    }
    threads.push((Thread::Contact(presage::libsignal_service::protocol::ServiceId::Aci(me.into())), "Note to Self".into()));
    for (t, title) in threads {
        let tl = Timeline::from_newest_first(me, store.messages(&t, ..).await?.flatten(), usize::MAX);
        if tl.messages.is_empty() { continue }
        let m = &tl.messages;
        let count = |f: &dyn Fn(&signal_rs::timeline::Message) -> bool| m.iter().filter(|x| f(x)).count();
        println!(
            "{title}: {} msgs ({} out), {} reactions, {} styled, {} quotes, {} attachments, {} edited, {} deleted, {} system",
            m.len(), count(&|x| x.outgoing), m.iter().map(|x| x.reactions.len()).sum::<usize>(),
            count(&|x| !x.ranges.is_empty()), count(&|x| x.quote.is_some()), m.iter().map(|x| x.attachments.len()).sum::<usize>(),
            count(&|x| x.edited), count(&|x| x.deleted), count(&|x| x.kind != Kind::Normal),
        );
    }
    Ok(())
}
