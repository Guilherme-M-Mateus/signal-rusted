//! Live checks against Signal's production servers. Run with `cargo test --test live -- --ignored`.
//! They create nothing on any account: an unscanned link code just expires.

use futures::channel::oneshot;
use presage::libsignal_service::configuration::SignalServers;
use presage::model::identity::OnNewIdentity;
use presage::Manager;
use presage_store_sqlite::SqliteStore;

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs network"]
async fn production_issues_a_link_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("probe.db3");
    let store = SqliteStore::open_with_passphrase(&db.to_string_lossy(), Some("probe"), OnNewIdentity::Trust)
        .await
        .unwrap();
    let (tx, rx) = oneshot::channel();
    let local = tokio::task::LocalSet::new();
    let url = local
        .run_until(async move {
            let link = tokio::task::spawn_local(Manager::link_secondary_device(
                store,
                SignalServers::Production,
                "probe".into(),
                tx,
            ));
            let url = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await;
            link.abort(); // never complete the link
            url
        })
        .await
        .expect("link code within 30s")
        .expect("channel open");
    let s = url.to_string();
    assert!(s.starts_with("sgnl://linkdevice?"), "{s}");
    assert!(s.contains("uuid=") && s.contains("pub_key="), "{s}");
}
