use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::model::{Subscription, TenantDescription};

const MAX_AGE: u64 = 7 * 24 * 60 * 60;
const VERSION: u32 = 2;

pub struct Cache {
    directory: PathBuf,
    source: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    version: u32,
    saved_at: u64,
    source: String,
    pub subscriptions: Vec<Subscription>,
    pub descriptions: Vec<TenantDescription>,
}

impl Cache {
    pub fn for_azure(executable: &OsStr) -> Option<Self> {
        let directory = env::var_os("AZURE_CONFIG_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
                env::var_os(home).map(|home| PathBuf::from(home).join(".azure"))
            })?;
        Some(Self::new(directory, executable))
    }

    fn new(directory: PathBuf, executable: &OsStr) -> Self {
        Self {
            directory,
            source: encode_source(executable),
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
            source: &'a str,
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
            source: &self.source,
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

fn encode_source(executable: &OsStr) -> String {
    if let Some(utf8) = executable.to_str() {
        return format!("utf8:{utf8}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = executable.as_bytes();
        let mut encoded = String::with_capacity(6 + bytes.len() * 2);
        encoded.push_str("bytes:");
        for byte in bytes {
            let _ = write!(encoded, "{byte:02x}");
        }
        encoded
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut encoded = String::from("wide:");
        for unit in executable.encode_wide() {
            let _ = write!(encoded, "{unit:04x}");
        }
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::Cache;
    use crate::model::{Subscription, TenantDescription};
    use std::{
        ffi::OsStr,
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
        let cache = Cache::new(directory.0.clone(), OsStr::new("az"));
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
        let cache = Cache::new(directory.0.clone(), OsStr::new("az"));
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
        let mut v1_numeric_source = valid.clone();
        v1_numeric_source["version"] = 1.into();
        v1_numeric_source["source"] = 12345.into();
        fs::write(&path, serde_json::to_vec(&v1_numeric_source).unwrap()).unwrap();
        assert!(cache.load().is_none());
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
        let cache = Cache::new(first.0.clone(), OsStr::new("az"));
        cache.save(&subscriptions(), &descriptions()).unwrap();
        assert!(
            Cache::new(second.0.clone(), OsStr::new("az"))
                .load()
                .is_none()
        );
        assert!(
            Cache::new(first.0.clone(), OsStr::new("different-az"))
                .load()
                .is_none()
        );
    }

    #[test]
    fn failed_write_preserves_previous_snapshot() {
        let directory = Directory::new();
        let cache = Cache::new(directory.0.clone(), OsStr::new("az"));
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

    #[test]
    fn stores_deterministic_string_source_and_distinguishes_non_utf8_executables() {
        let directory = Directory::new();
        let cache = Cache::new(directory.0.clone(), OsStr::new("/opt/az/bin/az"));
        cache.save(&subscriptions(), &descriptions()).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.0.join("azc-picker-cache.json")).unwrap())
                .unwrap();
        assert_eq!(raw["version"], 2);
        assert_eq!(raw["source"], "utf8:/opt/az/bin/az");

        let forged = Cache::new(
            directory.0.clone(),
            OsStr::new("bytes:2f6f70742f617a2f62696e2f617a"),
        );
        assert!(forged.load().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let first = Cache::new(directory.0.clone(), OsStr::from_bytes(&[b'a', b'z', 0xfe]));
            first.save(&subscriptions(), &descriptions()).unwrap();
            let raw_bytes: serde_json::Value = serde_json::from_slice(
                &fs::read(directory.0.join("azc-picker-cache.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(raw_bytes["source"], "bytes:617afe");
            assert!(first.load().is_some());

            let second = Cache::new(directory.0.clone(), OsStr::from_bytes(&[b'a', b'z', 0xff]));
            assert!(second.load().is_none());
        }
    }
}
