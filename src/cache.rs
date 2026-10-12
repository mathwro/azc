use std::{
    collections::hash_map::DefaultHasher,
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufReader, BufWriter, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::model::{Subscription, TenantDescription};

const MAX_AGE: u64 = 7 * 24 * 60 * 60;
const VERSION: u32 = 1;

pub struct Cache {
    directory: PathBuf,
    source: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    version: u32,
    saved_at: u64,
    source: u64,
    pub subscriptions: Vec<Subscription>,
    pub descriptions: Vec<TenantDescription>,
}

impl Cache {
    pub fn from_env() -> Option<Self> {
        let directory = env::var_os("AZURE_CONFIG_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
                env::var_os(home).map(|home| PathBuf::from(home).join(".azure"))
            })?;
        Some(Self::new(
            directory,
            env::var_os("AZC_AZ_BIN").unwrap_or_else(|| OsString::from("az")),
        ))
    }

    fn new(directory: PathBuf, executable: OsString) -> Self {
        let mut source = DefaultHasher::new();
        executable.hash(&mut source);
        Self {
            directory,
            source: source.finish(),
        }
    }

    pub fn load(&self) -> Option<Snapshot> {
        let file = fs::File::open(self.directory.join("azc-picker-cache.json")).ok()?;
        let snapshot: Snapshot = serde_json::from_reader(BufReader::new(file)).ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let age = now.checked_sub(snapshot.saved_at)?;
        (snapshot.version == VERSION && snapshot.source == self.source && age <= MAX_AGE)
            .then_some(snapshot)
    }

    pub fn save(
        &self,
        subscriptions: &[Subscription],
        descriptions: &[TenantDescription],
    ) -> io::Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Record<'a> {
            version: u32,
            saved_at: u64,
            source: u64,
            subscriptions: &'a [Subscription],
            descriptions: &'a [TenantDescription],
        }
        let saved_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs();
        let record = Record {
            version: VERSION,
            saved_at,
            source: self.source,
            subscriptions,
            descriptions,
        };
        fs::create_dir_all(&self.directory)?;
        let temporary = self
            .directory
            .join(format!("azc-picker-cache.{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temporary)?;
        let result = (|| {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, &record)?;
            writer.flush()?;
            // Close before replacement so Windows does not keep the staging file locked.
            drop(writer);
            fs::rename(&temporary, self.directory.join("azc-picker-cache.json"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::Cache;
    use crate::model::{Subscription, TenantDescription};
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "azc-cache-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn subscriptions() -> Vec<Subscription> {
        serde_json::from_str(r#"[{"id":"alpha","name":"Alpha","tenantId":"tenant-a","isDefault":true},{"id":"beta","name":"Beta","tenantId":"tenant-b"}]"#).unwrap()
    }
    fn descriptions() -> Vec<TenantDescription> {
        serde_json::from_str(r#"[{"tenantId":"tenant-a","displayName":"Tenant A"},{"tenantId":"tenant-b","displayName":"Tenant B","defaultDomain":"b.example"}]"#).unwrap()
    }

    #[test]
    fn persists_subscription_ids_and_tenant_labels_for_next_run() {
        let directory = Directory::new();
        let cache = Cache::new(directory.0.clone(), OsString::from("az"));
        cache.save(&subscriptions(), &descriptions()).unwrap();
        let restored = cache.load().unwrap();
        assert_eq!(restored.subscriptions[1].id, "beta");
        assert_eq!(restored.subscriptions[0].tenant_id, "tenant-a");
        assert!(restored.subscriptions[0].is_default);
        assert_eq!(restored.descriptions[0].display_name, "Tenant A");
        assert_eq!(restored.descriptions[1].display_name, "Tenant B");
        assert_eq!(restored.descriptions[1].default_domain, "b.example");
    }

    #[test]
    fn ignores_expired_future_corrupt_and_incompatible_snapshots() {
        let directory = Directory::new();
        let cache = Cache::new(directory.0.clone(), OsString::from("az"));
        assert!(cache.load().is_none());
        cache.save(&subscriptions(), &descriptions()).unwrap();
        let path = directory.0.join("azc-picker-cache.json");
        let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for timestamp in [now - 8 * 24 * 60 * 60, now + 24 * 60 * 60] {
            let mut snapshot = valid.clone();
            snapshot["savedAt"] = timestamp.into();
            fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            assert!(cache.load().is_none());
        }
        let mut incompatible = valid;
        incompatible["version"] = 999.into();
        fs::write(&path, serde_json::to_vec(&incompatible).unwrap()).unwrap();
        assert!(cache.load().is_none());
        fs::write(&path, b"{broken").unwrap();
        assert!(cache.load().is_none());
    }

    #[test]
    fn isolates_azure_configurations_and_executable_overrides() {
        let first = Directory::new();
        let second = Directory::new();
        let cache = Cache::new(first.0.clone(), OsString::from("az"));
        cache.save(&subscriptions(), &descriptions()).unwrap();
        assert!(
            Cache::new(second.0.clone(), OsString::from("az"))
                .load()
                .is_none()
        );
        assert!(
            Cache::new(first.0.clone(), OsString::from("different-az"))
                .load()
                .is_none()
        );
    }

    #[test]
    fn failed_write_preserves_previous_snapshot() {
        let directory = Directory::new();
        let cache = Cache::new(directory.0.clone(), OsString::from("az"));
        cache.save(&subscriptions(), &descriptions()).unwrap();
        // Blocking the staging path must not truncate the usable cache.
        fs::create_dir(
            directory
                .0
                .join(format!("azc-picker-cache.{}.tmp", std::process::id())),
        )
        .unwrap();
        assert!(cache.save(&[], &[]).is_err());
        assert_eq!(cache.load().unwrap().subscriptions[1].id, "beta");
    }
}
