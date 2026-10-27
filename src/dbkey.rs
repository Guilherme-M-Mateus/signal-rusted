//! The SQLCipher database key: kept in the OS keyring (Secret Service on Linux), with the
//! legacy owner-only `db.key` file as the fallback when no keyring is running. Losing the key
//! means losing the database, so the file is only removed once the keyring reads it back.

use std::path::Path;

/// Where the key lives; `Err` means the keyring itself is unavailable.
pub trait KeyStore {
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, key: &str) -> Result<(), String>;
}

/// One keyring entry per data directory, so separate profiles keep separate keys.
pub struct OsKeyring(Result<keyring::Entry, String>);

impl OsKeyring {
    pub fn new(data_dir: &Path) -> Self {
        OsKeyring(keyring::Entry::new("signal-rs", &data_dir.to_string_lossy()).map_err(|e| e.to_string()))
    }
}

impl KeyStore for OsKeyring {
    fn get(&self) -> Result<Option<String>, String> {
        match self.0.as_ref()?.get_password() {
            Ok(k) => Ok(Some(k)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, key: &str) -> Result<(), String> {
        self.0.as_ref()?.set_password(key).map_err(|e| e.to_string())
    }
}

/// Stores `key` in the keyring and checks it reads back unchanged.
fn store_verified(keys: &dyn KeyStore, key: &str) -> Result<(), String> {
    keys.set(key)?;
    match keys.get()? {
        Some(k) if k == key => Ok(()),
        _ => Err("keyring did not return the stored key".into()),
    }
}

/// The database key: from the keyring, migrating a legacy `db.key` into it, or new.
pub fn db_passphrase(data_dir: &Path, keys: &dyn KeyStore) -> anyhow::Result<String> {
    let path = data_dir.join("db.key");
    let file = std::fs::read_to_string(&path).ok().map(|k| k.trim().to_owned());
    let db_exists = data_dir.join("signal.db3").exists();
    match (keys.get(), file) {
        (Ok(Some(k)), None) => Ok(k),
        (Ok(Some(k)), Some(f)) if k == f => {
            // A migration that stopped before deleting the file.
            remove(&path);
            Ok(k)
        }
        (Ok(Some(_)), Some(f)) => {
            // The file is only ever written without a keyring key, so it is the newer one.
            tracing::warn!("keyring and db.key hold different keys; using db.key and keeping both");
            Ok(f)
        }
        (Ok(None), Some(f)) => {
            match store_verified(keys, &f) {
                Ok(()) => {
                    remove(&path);
                    tracing::info!("moved the database key into the OS keyring");
                }
                Err(e) => tracing::warn!("couldn't move the database key into the OS keyring ({e}); keeping db.key"),
            }
            Ok(f)
        }
        (Err(e), Some(f)) => {
            tracing::warn!("OS keyring unavailable ({e}); using db.key");
            Ok(f)
        }
        (got, None) => {
            // A new key would make an existing database unreadable: never paper over that.
            anyhow::ensure!(!db_exists, "the database key is missing: {}", got.err().unwrap_or_else(|| "not in the OS keyring and no db.key".into()));
            let key = hex::encode(rand::random::<[u8; 32]>());
            if let Err(e) = got.and_then(|_| store_verified(keys, &key)) {
                tracing::warn!("OS keyring unavailable ({e}); storing the database key in db.key");
                crate::backend::write_private(&path, &key)?;
            }
            Ok(key)
        }
    }
}

fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!("couldn't delete {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory keyring; `None` behaves like a missing Secret Service, `lossy` drops writes.
    struct Fake {
        key: RefCell<Option<Option<String>>>,
        lossy: bool,
    }

    impl Fake {
        fn empty() -> Self {
            Fake { key: RefCell::new(Some(None)), lossy: false }
        }
        fn unavailable() -> Self {
            Fake { key: RefCell::new(None), lossy: false }
        }
    }

    impl KeyStore for Fake {
        fn get(&self) -> Result<Option<String>, String> {
            self.key.borrow().clone().ok_or_else(|| "no secret service".into())
        }
        fn set(&self, key: &str) -> Result<(), String> {
            let mut k = self.key.borrow_mut();
            let slot = k.as_mut().ok_or("no secret service")?;
            if !self.lossy {
                *slot = Some(key.into());
            }
            Ok(())
        }
    }

    fn legacy(dir: &Path, key: &str) {
        crate::backend::write_private(&dir.join("db.key"), key).unwrap();
        std::fs::write(dir.join("signal.db3"), b"").unwrap();
    }

    #[test]
    fn new_key_goes_to_the_keyring_only() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Fake::empty();
        let a = db_passphrase(dir.path(), &keys).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(keys.get().unwrap().as_deref(), Some(a.as_str()));
        assert!(!dir.path().join("db.key").exists());
        std::fs::write(dir.path().join("signal.db3"), b"").unwrap();
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), a);
    }

    #[test]
    fn legacy_file_moves_into_the_keyring_then_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        legacy(dir.path(), "abc\n");
        let keys = Fake::empty();
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), "abc");
        assert_eq!(keys.get().unwrap().as_deref(), Some("abc"));
        assert!(!dir.path().join("db.key").exists());
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), "abc", "next start reads the keyring");
    }

    #[test]
    fn file_is_kept_when_the_keyring_does_not_read_back() {
        let dir = tempfile::tempdir().unwrap();
        legacy(dir.path(), "abc");
        let keys = Fake { lossy: true, ..Fake::empty() };
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), "abc");
        assert!(dir.path().join("db.key").exists());
    }

    #[test]
    fn no_keyring_falls_back_to_the_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Fake::unavailable();
        let a = db_passphrase(dir.path(), &keys).unwrap();
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("db.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        legacy(dir.path(), &a);
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), a, "existing file, keyring still down");
    }

    #[test]
    fn interrupted_migration_and_conflicts_never_drop_a_key() {
        let dir = tempfile::tempdir().unwrap();
        legacy(dir.path(), "abc");
        let keys = Fake::empty();
        keys.set("abc").unwrap();
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), "abc");
        assert!(!dir.path().join("db.key").exists(), "same key: finish the migration");

        legacy(dir.path(), "file");
        assert_eq!(db_passphrase(dir.path(), &keys).unwrap(), "file");
        assert!(dir.path().join("db.key").exists(), "different keys: keep the file");
        assert_eq!(keys.get().unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn existing_database_without_its_key_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("signal.db3"), b"").unwrap();
        assert!(db_passphrase(dir.path(), &Fake::unavailable()).is_err());
        let keys = Fake::empty();
        assert!(db_passphrase(dir.path(), &keys).is_err());
        assert_eq!(keys.get().unwrap(), None, "nothing written");
        assert!(!dir.path().join("db.key").exists());
    }
}
