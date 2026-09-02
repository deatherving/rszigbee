//! An on-disk [`ZigbeeStore`].
//!
//! Layout:
//!
//! ```text
//! <root>/
//! ├── schema            a single integer, so a future format change is detectable
//! ├── network.json      network identity and the frame counter
//! ├── devices/
//! │   └── 0x00124b0022189abc.json
//! ├── groups/
//! │   └── 901.json
//! └── backups/
//!     └── 1756789523123-0000-ember-0x94a081fffed96e5c.json
//! ```
//!
//! # One file per device, not one file for all devices
//!
//! zigbee-herdsman rewrites its entire database on any change. At a thousand
//! devices that is a multi-megabyte write and fsync every time a device is
//! merely heard from. Per-device files make an update proportional to what
//! changed, and make a single corrupt device recoverable by quarantining one
//! file instead of losing the inventory.
//!
//! # Writes are atomic
//!
//! Every write goes to `<name>.tmp`, is fsynced, then renamed over the target.
//! A crash mid-write therefore leaves either the old file or the new one, never
//! a half-written one. The directory is fsynced after a rename so the rename
//! itself is durable.
//!
//! # Corruption is handled by what was corrupt
//!
//! A corrupt *device* file is moved aside with a timestamp and startup
//! continues: losing one device's cached record is recoverable by
//! re-interviewing it. A corrupt *network* file stops startup, because
//! continuing means forming a new network and orphaning every joined device —
//! the one outcome this project treats as unacceptable.

use std::path::{Path, PathBuf};

use rszigbee_spec::ids::{GroupId, Ieee};
use tracing::{debug, error, warn};

use super::{
    BackupMeta, PersistedDevice, PersistedGroup, PersistedNetwork, StoreError, ZigbeeStore,
};

/// The on-disk format version. Bumping this requires a migration.
const SCHEMA: u32 = 1;

/// A [`ZigbeeStore`] backed by a directory.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    /// Opens (and creates, if absent) a store rooted at `root`.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created, or if it holds a schema
    /// version this build does not understand — reading a future format with
    /// old code is how data gets silently mangled.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        for sub in ["", "devices", "groups", "backups"] {
            tokio::fs::create_dir_all(root.join(sub))
                .await
                .map_err(|e| StoreError::Io(format!("cannot create {}: {e}", root.display())))?;
        }

        let schema_path = root.join("schema");
        match tokio::fs::read_to_string(&schema_path).await {
            Ok(text) => {
                let found: u32 = text.trim().parse().map_err(|_| StoreError::Corrupt {
                    location: schema_path.display().to_string(),
                    detail: format!("expected an integer, found {:?}", text.trim()),
                })?;
                if found > SCHEMA {
                    return Err(StoreError::Corrupt {
                        location: schema_path.display().to_string(),
                        detail: format!(
                            "on-disk schema is version {found}, this build understands {SCHEMA}. \
                             Refusing to read a newer format rather than risk mangling it."
                        ),
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(&schema_path, SCHEMA.to_string().as_bytes()).await?;
            }
            Err(e) => {
                return Err(StoreError::Io(format!(
                    "cannot read the schema marker: {e}"
                )));
            }
        }

        debug!(root = %root.display(), "file store open");
        Ok(Self { root })
    }

    /// The directory this store uses.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn network_path(&self) -> PathBuf {
        self.root.join("network.json")
    }

    fn device_path(&self, ieee: Ieee) -> PathBuf {
        // The canonical hex form, so the directory is browsable and greppable
        // by the address a user sees in logs and MQTT topics.
        self.root.join("devices").join(format!("{ieee}.json"))
    }

    fn group_path(&self, id: GroupId) -> PathBuf {
        self.root.join("groups").join(format!("{}.json", id.0))
    }

    /// Reads and parses every `.json` file in a directory.
    ///
    /// A file that fails to parse is quarantined and skipped rather than
    /// failing the whole load: one unreadable device record must not cost the
    /// entire inventory.
    async fn load_dir<T: serde::de::DeserializeOwned>(
        &self,
        sub: &str,
    ) -> Result<Vec<T>, StoreError> {
        let dir = self.root.join(sub);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(StoreError::Io(format!(
                    "cannot list {}: {e}",
                    dir.display()
                )));
            }
        };

        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StoreError::Io(format!("cannot walk {}: {e}", dir.display())))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            match read_json::<T>(&path).await {
                Ok(v) => out.push(v),
                Err(e) => {
                    error!(path = %path.display(), error = %e, "quarantining a corrupt record");
                    quarantine(&path).await;
                }
            }
        }
        Ok(out)
    }
}

impl ZigbeeStore for FileStore {
    async fn load_network(&self) -> Result<Option<PersistedNetwork>, StoreError> {
        let path = self.network_path();
        match read_json::<PersistedNetwork>(&path).await {
            Ok(n) => Ok(Some(n)),
            Err(StoreError::Io(_)) if !path.exists() => Ok(None),
            // Deliberately *not* quarantined. Continuing without network
            // identity means forming a new network and orphaning every joined
            // device, so this has to stop the caller.
            Err(e) => Err(StoreError::Corrupt {
                location: path.display().to_string(),
                detail: format!(
                    "{e}. Network identity cannot be regenerated: without it the \
                     network is lost. Restore a backup rather than deleting this file."
                ),
            }),
        }
    }

    async fn save_network(&self, network: &PersistedNetwork) -> Result<(), StoreError> {
        write_json(&self.network_path(), network).await
    }

    async fn load_devices(&self) -> Result<Vec<PersistedDevice>, StoreError> {
        self.load_dir("devices").await
    }

    async fn upsert_device(&self, device: &PersistedDevice) -> Result<(), StoreError> {
        write_json(&self.device_path(device.ieee), device).await
    }

    async fn delete_device(&self, ieee: Ieee) -> Result<(), StoreError> {
        remove_if_present(&self.device_path(ieee)).await
    }

    async fn load_groups(&self) -> Result<Vec<PersistedGroup>, StoreError> {
        self.load_dir("groups").await
    }

    async fn upsert_group(&self, group: &PersistedGroup) -> Result<(), StoreError> {
        write_json(&self.group_path(group.id), group).await
    }

    async fn delete_group(&self, id: GroupId) -> Result<(), StoreError> {
        remove_if_present(&self.group_path(id)).await
    }

    async fn save_backup(
        &self,
        adapter: &str,
        coordinator: Ieee,
        bytes: &[u8],
    ) -> Result<String, StoreError> {
        // Timestamped and never overwritten. A backup history is the difference
        // between "restore the good one" and "the only backup is the bad one".
        let id = format!("{}-{adapter}-{coordinator}", timestamp());
        let path = self.root.join("backups").join(format!("{id}.json"));
        write_atomic(&path, bytes).await?;

        // A backup holds the network key and every device link key. Owner-only.
        restrict_permissions(&path).await;
        Ok(id)
    }

    async fn list_backups(&self) -> Result<Vec<BackupMeta>, StoreError> {
        let dir = self.root.join("backups");
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(format!("cannot list backups: {e}"))),
        };

        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StoreError::Io(format!("cannot walk backups: {e}")))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // The id encodes everything a listing needs, so there is no side
            // index that could drift out of step with the files themselves.
            match parse_backup_id(id) {
                Some(meta) => out.push(meta),
                None => {
                    warn!(path = %path.display(), "skipping a backup with an unrecognised name");
                }
            }
        }
        // Newest first. The id starts with zero-padded epoch milliseconds, so
        // it is fixed-width for any date this code will see and lexical order
        // is chronological order.
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    async fn load_backup(&self, id: &str) -> Result<Vec<u8>, StoreError> {
        // `id` reaches here from an MQTT request in a gateway deployment, so it
        // is untrusted: reject anything that could escape the directory.
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err(StoreError::BackupNotFound(id.to_owned()));
        }
        let path = self.root.join("backups").join(format!("{id}.json"));
        tokio::fs::read(&path)
            .await
            .map_err(|_| StoreError::BackupNotFound(id.to_owned()))
    }

    async fn flush(&self) -> Result<(), StoreError> {
        // Every write is already fsynced and renamed before returning, so there
        // is nothing buffered. Kept as a no-op rather than removed so a
        // batching backend can implement it meaningfully.
        Ok(())
    }
}

/// Parses a backup filename back into its metadata.
///
/// The format is `{millis}-{counter}-{adapter}-{coordinator}`. Parsed from the
/// right, because an adapter name could itself contain a hyphen while the
/// millis, counter and address never do.
fn parse_backup_id(id: &str) -> Option<BackupMeta> {
    let (head, coordinator) = id.rsplit_once('-')?;
    let coordinator_ieee = Ieee::parse(coordinator).ok()?;
    let (stamp, adapter) = head.rsplit_once('-')?;
    let (millis, _counter) = stamp.rsplit_once('-')?;

    Some(BackupMeta {
        id: id.to_owned(),
        taken_epoch_ms: millis.parse().ok()?,
        adapter: adapter.to_owned(),
        coordinator_ieee,
    })
}

/// Reads and deserialises one JSON file.
async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| StoreError::Io(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
        location: path.display().to_string(),
        detail: e.to_string(),
    })
}

/// Serialises and writes one JSON file atomically.
async fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| StoreError::Io(format!("cannot serialise {}: {e}", path.display())))?;
    write_atomic(path, &bytes).await
}

/// Writes to a temporary file, fsyncs it, then renames it over the target.
///
/// The fsync before the rename is what makes the guarantee real: without it a
/// crash can leave the rename durable but the contents not.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    let io =
        |what: &str, e: std::io::Error| StoreError::Io(format!("{what} {}: {e}", path.display()));

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| io("cannot create a temporary file for", e))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, bytes)
        .await
        .map_err(|e| io("cannot write", e))?;
    file.sync_all().await.map_err(|e| io("cannot fsync", e))?;
    drop(file);

    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| io("cannot rename into place", e))?;

    // Fsync the directory so the rename itself survives a crash.
    if let Some(dir) = path.parent()
        && let Ok(handle) = tokio::fs::File::open(dir).await
    {
        let _ = handle.sync_all().await;
    }
    Ok(())
}

async fn remove_if_present(path: &Path) -> Result<(), StoreError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        // Deleting something already absent is the desired end state, not an
        // error: a caller removing a device twice should not have to care.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Io(format!(
            "cannot remove {}: {e}",
            path.display()
        ))),
    }
}

/// Moves an unreadable file aside so the next load does not trip over it.
async fn quarantine(path: &Path) {
    let aside = path.with_extension(format!("corrupt-{}", timestamp()));
    match tokio::fs::rename(path, &aside).await {
        Ok(()) => {
            warn!(from = %path.display(), to = %aside.display(), "quarantined");
        }
        Err(e) => {
            error!(
                path = %path.display(),
                error = %e,
                "cannot quarantine; it will be skipped again on the next load"
            );
        }
    }
}

/// Restricts a file to its owner. Best effort: a store on a filesystem without
/// Unix permissions still works, it just cannot enforce this.
async fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
        {
            warn!(
                path = %path.display(),
                error = %e,
                "cannot restrict permissions on a file holding key material"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// A filesystem-safe, lexically sortable, unique stamp.
///
/// Millisecond resolution **plus** a process counter. Seconds alone are not
/// enough and the first version of this was wrong because of it: two backups
/// taken in the same second collided on the id and the second silently
/// overwrote the first, which defeats the entire point of keeping a history.
/// The counter closes the remaining sub-millisecond window.
fn timestamp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // The counter is what makes two backups taken in the same millisecond
    // distinct. Without it the second silently overwrote the first, which was
    // observed at one-second resolution.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:013}-{n:04}", super::epoch_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::InterviewState;
    use rszigbee_spec::ids::{EndpointId, Nwk};

    async fn store() -> (FileStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = FileStore::open(dir.path()).await.expect("open");
        (s, dir)
    }

    #[tokio::test]
    async fn the_file_store_conforms() {
        // The backend-independent promises, asserted by the same suite that
        // runs against MemoryStore. The tests below cover what is specific to
        // this backend: atomicity, quarantine, schema refusal, permissions.
        let (s, _dir) = store().await;
        crate::store::conformance::assert_conforms(&s).await;
    }

    fn network() -> PersistedNetwork {
        PersistedNetwork {
            pan_id: 0x879b,
            extended_pan_id: 0x94a0_81ff_fed9_6e5c,
            channel: 11,
            nwk_update_id: 0,
            coordinator_ieee: Ieee::new(0x94a0_81ff_fed9_6e5c),
            key_sequence: 0,
            frame_counter: 12_345,
        }
    }

    #[tokio::test]
    async fn a_fresh_store_reports_no_network_rather_than_failing() {
        let (s, _d) = store().await;
        assert_eq!(s.load_network().await.unwrap(), None);
        assert!(s.load_devices().await.unwrap().is_empty());
        assert!(s.list_backups().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn network_identity_survives_a_reopen_including_the_frame_counter() {
        // The whole point of persistence: losing the frame counter breaks the
        // network, because replay protection rejects a counter it has seen.
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let s = FileStore::open(dir.path()).await.unwrap();
            s.save_network(&network()).await.unwrap();
        }
        let s = FileStore::open(dir.path()).await.unwrap();
        let got = s.load_network().await.unwrap().expect("present");
        assert_eq!(got, network());
        assert_eq!(got.frame_counter, 12_345);
    }

    #[tokio::test]
    async fn devices_round_trip_and_are_stored_one_file_each() {
        let (s, dir) = store().await;
        let a = PersistedDevice::new(Ieee::new(0x0017_8801_00dc_4d3f), Nwk::new(0x1234));
        let b = PersistedDevice::new(Ieee::new(0x0012_4b00_2218_9abc), Nwk::new(0x5678));
        s.upsert_device(&a).await.unwrap();
        s.upsert_device(&b).await.unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path().join("devices"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        files.sort();
        assert_eq!(
            files,
            ["0x00124b0022189abc.json", "0x0017880100dc4d3f.json"],
            "one file per device, named by the address a user sees"
        );

        assert_eq!(s.load_devices().await.unwrap().len(), 2);
        s.delete_device(a.ieee).await.unwrap();
        assert_eq!(s.load_devices().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_an_absent_device_is_not_an_error() {
        // Removing a device twice should not force the caller to care.
        let (s, _d) = store().await;
        s.delete_device(Ieee::new(1)).await.unwrap();
        s.delete_group(GroupId(9)).await.unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_device_file_is_quarantined_and_the_rest_still_load() {
        // One unreadable record must not cost the whole inventory.
        let (s, dir) = store().await;
        let good = PersistedDevice::new(Ieee::new(0x0017_8801_00dc_4d3f), Nwk::new(1));
        s.upsert_device(&good).await.unwrap();
        std::fs::write(
            dir.path().join("devices/0xdeadbeefdeadbeef.json"),
            b"{ not json",
        )
        .unwrap();

        let loaded = s.load_devices().await.unwrap();
        assert_eq!(loaded.len(), 1, "the good record must still load");
        assert_eq!(loaded.first().map(|d| d.ieee), Some(good.ieee));

        // And the bad file is moved aside so the next load is clean.
        assert!(!dir.path().join("devices/0xdeadbeefdeadbeef.json").exists());
        let quarantined = std::fs::read_dir(dir.path().join("devices"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(
            quarantined,
            "the corrupt file should have been renamed aside"
        );
    }

    #[tokio::test]
    async fn a_corrupt_network_file_stops_the_caller_rather_than_being_quarantined() {
        // The asymmetry that matters. Continuing without network identity means
        // forming a new network and orphaning every device.
        let (s, dir) = store().await;
        std::fs::write(dir.path().join("network.json"), b"{ not json").unwrap();
        let e = s.load_network().await.expect_err("must refuse");
        assert!(matches!(e, StoreError::Corrupt { .. }), "got {e:?}");
        assert!(e.to_string().contains("cannot be regenerated"));
        // And it is NOT quarantined: the file is still there to be recovered.
        assert!(dir.path().join("network.json").exists());
    }

    #[tokio::test]
    async fn a_future_schema_version_is_refused() {
        // Reading a newer format with older code silently mangles data.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("schema"), b"99").unwrap();
        let e = FileStore::open(dir.path()).await.expect_err("must refuse");
        assert!(e.to_string().contains("version 99"), "{e}");
    }

    #[tokio::test]
    async fn an_unparsable_schema_marker_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("schema"), b"banana").unwrap();
        assert!(FileStore::open(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn backups_are_kept_as_history_and_are_owner_only() {
        let (s, _d) = store().await;
        let coord = Ieee::new(0x94a0_81ff_fed9_6e5c);
        let first = s.save_backup("ember", coord, b"one").await.unwrap();
        let second = s.save_backup("ember", coord, b"two").await.unwrap();

        // A restore needs a specific backup, not the newest.
        assert_eq!(s.load_backup(&first).await.unwrap(), b"one");
        assert_eq!(s.load_backup(&second).await.unwrap(), b"two");
        assert_eq!(s.list_backups().await.unwrap().len(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = s.root().join("backups").join(format!("{first}.json"));
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a backup holds the network key");
        }
    }

    #[test]
    fn a_backup_id_round_trips_through_its_metadata() {
        let id = "1756789523123-0007-ember-0x94a081fffed96e5c";
        let m = parse_backup_id(id).expect("parses");
        assert_eq!(m.id, id);
        assert_eq!(m.taken_epoch_ms, 1_756_789_523_123);
        assert_eq!(m.adapter, "ember");
        assert_eq!(m.coordinator_ieee, Ieee::new(0x94a0_81ff_fed9_6e5c));
    }

    #[test]
    fn a_backup_name_that_makes_no_sense_is_skipped_not_guessed() {
        // A stray file in the backups directory must not become a phantom
        // backup that a restore would then fail on.
        for bad in [
            "",
            "nonsense",
            "1-2-3",
            "notmillis-0000-ember-0xdeadbeefdeadbeef",
        ] {
            assert!(parse_backup_id(bad).is_none(), "{bad:?} should not parse");
        }
    }

    #[tokio::test]
    async fn backup_metadata_is_recovered_from_the_filename() {
        let (s, _d) = store().await;
        let coord = Ieee::new(0x94a0_81ff_fed9_6e5c);
        s.save_backup("ember", coord, b"x").await.unwrap();
        let listed = s.list_backups().await.unwrap();
        let m = listed.first().expect("one backup");
        assert_eq!(m.adapter, "ember");
        assert_eq!(m.coordinator_ieee, coord);
        assert!(m.taken_epoch_ms > 1_700_000_000_000, "a real timestamp");
    }

    #[tokio::test]
    async fn two_backups_in_the_same_millisecond_do_not_collide() {
        // The bug this guards was real: with second resolution the second
        // backup silently overwrote the first, which defeats keeping history.
        let (s, _d) = store().await;
        let coord = Ieee::new(1);
        let mut ids = Vec::new();
        for i in 0..20u8 {
            ids.push(s.save_backup("ember", coord, &[i]).await.unwrap());
        }
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "backup ids must be unique");
        // And every one is still readable with its own contents.
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                s.load_backup(id).await.unwrap(),
                vec![u8::try_from(i).unwrap()]
            );
        }
    }

    #[tokio::test]
    async fn a_backup_id_cannot_escape_the_directory() {
        // In a gateway, this id arrives from an MQTT request.
        let (s, _d) = store().await;
        for evil in ["../../etc/passwd", "..", "a/b", "a\\b"] {
            assert!(
                matches!(
                    s.load_backup(evil).await,
                    Err(StoreError::BackupNotFound(_))
                ),
                "{evil} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn passthrough_fields_survive_the_round_trip() {
        // What makes a Zigbee2MQTT import lossless and a rollback possible.
        let (s, _d) = store().await;
        let mut d = PersistedDevice::new(Ieee::new(7), Nwk::new(7));
        d.passthrough.insert("someFutureField".into(), "42".into());
        d.interview = InterviewState::Successful;
        s.upsert_device(&d).await.unwrap();

        let back = s.load_devices().await.unwrap();
        let got = back.first().expect("device");
        assert_eq!(
            got.passthrough.get("someFutureField").map(String::as_str),
            Some("42")
        );
        assert_eq!(got.interview, InterviewState::Successful);
    }

    #[tokio::test]
    async fn sixty_four_bit_values_are_stored_as_hex_not_numbers() {
        // An extended PAN id is routinely above 2^53. Written as a bare JSON
        // number it cannot round-trip through a consumer that uses doubles, and
        // it corrupts silently rather than failing.
        let (s, _d) = store().await;
        s.save_network(&network()).await.unwrap();
        let text = std::fs::read_to_string(s.root().join("network.json")).unwrap();
        assert!(text.contains("\"0x94a081fffed96e5c\""), "{text}");
        assert!(
            !text.contains("10709702850379345500"),
            "a value above 2^53 must not be written as a number: {text}"
        );
        // And it reads back exactly.
        let back = s.load_network().await.unwrap().expect("present");
        assert_eq!(back.extended_pan_id, 0x94a0_81ff_fed9_6e5c);
    }

    #[tokio::test]
    async fn an_ieee_is_stored_as_its_canonical_string() {
        // So the file is greppable by the address a user sees in logs and MQTT
        // topics, and so a Zigbee2MQTT import reads naturally.
        let (s, _d) = store().await;
        let d = PersistedDevice::new(Ieee::new(0x0017_8801_00dc_4d3f), Nwk::new(1));
        s.upsert_device(&d).await.unwrap();
        let text =
            std::fs::read_to_string(s.root().join("devices/0x0017880100dc4d3f.json")).unwrap();
        assert!(text.contains("\"0x0017880100dc4d3f\""), "{text}");
    }

    #[tokio::test]
    async fn groups_round_trip() {
        let (s, _d) = store().await;
        s.upsert_group(&PersistedGroup {
            id: GroupId(901),
            members: vec![(Ieee::new(1), EndpointId(1))],
        })
        .await
        .unwrap();
        assert_eq!(s.load_groups().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_temporary_files_are_left_behind() {
        // A stray .tmp is how the next open finds a half-written file.
        let (s, dir) = store().await;
        s.save_network(&network()).await.unwrap();
        s.upsert_device(&PersistedDevice::new(Ieee::new(1), Nwk::new(1)))
            .await
            .unwrap();
        for sub in ["", "devices"] {
            let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(sub))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "left a .tmp behind in {sub:?}");
        }
    }
}
