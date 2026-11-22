//! Durable, explicit recovery copies. These records are never replayed into source files.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context};
use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(windows)]
mod windows;

use crate::{
    snapshot::{RojoTree, SourceTransform},
    SessionId,
};

pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_COUNT: usize = 128;
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_RETIRED_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_RETIRED_COUNT: usize = 128;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const RETIRED_LIFETIME: std::time::Duration = std::time::Duration::from_secs(10 * 60);
pub(crate) const SCHEMA_VERSION: u32 = 1;
const INCOMPLETE_PREFIX: &str = ".rojo-checkpoint-incomplete-";
const INCOMPLETE_SUFFIX: &str = ".tmp";
const INCOMPLETE_RANDOM_LENGTH: usize = 32;

/// Only server-derived data can describe a recovery target. Ownership and reserved-path
/// facts are captured while its ancestors and context still exist. Package policy and
/// project syncback rules are checked again by the route, just as for live Sources.
#[derive(Clone, Debug)]
pub(crate) struct SourceDescriptor {
    pub id: Ref,
    pub name: String,
    pub class_name: rbx_dom_weak::Ustr,
    pub canonical_source: String,
    pub source_transform: Option<SourceTransform>,
    pub source_path: PathBuf,
    pub full_path: String,
    project_owned: bool,
    reserved_path: bool,
}

impl SourceDescriptor {
    pub fn capture(tree: &RojoTree, id: Ref) -> Result<Self, &'static str> {
        use rbx_dom_weak::{types::Variant, ustr};
        let instance = tree
            .get_instance(id)
            .ok_or("Checkpoint ID is not a tracked instance")?;
        let metadata = instance.metadata();
        let Some(Variant::String(source)) = instance.properties().get(&ustr("Source")) else {
            return Err("Checkpoint target must be a managed script with Source");
        };
        if !matches!(
            instance.class_name().as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            return Err("Checkpoint target must be a managed script");
        }
        let source_path = metadata
            .relevant_paths
            .iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "lua" || extension == "luau")
            })
            .cloned()
            .or_else(|| crate::syncback::source_file(&instance))
            .or_else(|| {
                metadata
                    .instigating_source
                    .as_ref()
                    .map(|source| source.path().to_path_buf())
                    .or_else(|| {
                        std::iter::successors(tree.get_instance(instance.parent()), |current| {
                            tree.get_instance(current.parent())
                        })
                        .find_map(|current| {
                            current
                                .metadata()
                                .instigating_source
                                .as_ref()
                                .map(|source| source.path().to_path_buf())
                        })
                    })
            })
            .ok_or("Checkpoint target has no managed source path")?;
        let owner = if metadata.instigating_source.is_some() {
            Some(metadata)
        } else {
            std::iter::successors(tree.get_instance(instance.parent()), |current| {
                tree.get_instance(current.parent())
            })
            .find(|current| current.metadata().instigating_source.is_some())
            .map(|current| current.metadata())
        };
        Ok(Self {
            id,
            name: instance.name().to_owned(),
            class_name: instance.class_name(),
            canonical_source: source.clone(),
            source_transform: metadata.source_transform.clone(),
            reserved_path: metadata.context.is_reserved_path(&source_path),
            project_owned: owner.is_some_and(|owner| {
                crate::syncback::is_project_owned(owner) && owner.context.package_mount.is_none()
            }),
            source_path,
            full_path: tree.inner().full_path_of(id, "/"),
        })
    }

    pub fn validate(
        &self,
        project: &crate::Project,
        tree: &RojoTree,
        vfs: &memofs::Vfs,
    ) -> anyhow::Result<bool> {
        use rbx_dom_weak::{ustr, InstanceBuilder, WeakDom};
        let property_tree = WeakDom::new(InstanceBuilder::new(self.class_name));
        let ignored =
            crate::syncback::get_property_filter(project, property_tree.root()).unwrap_or_default();
        let ignored_tree = project.syncback_rules.as_ref().is_some_and(|rules| {
            rules
                .ignore_trees
                .iter()
                .any(|ignored| self.full_path.starts_with(ignored))
        });
        if self.project_owned
            || ignored_tree
            || ignored.contains(&ustr("Source"))
            || !crate::syncback::can_write_property(project, self.class_name.as_str(), "Source")
        {
            return Ok(false);
        }
        let globs = project
            .syncback_rules
            .as_ref()
            .map(|rules| rules.compile_globs())
            .transpose()?;
        Ok(
            crate::syncback::is_valid_path(&globs, project.folder_location(), &self.source_path)
                && !self.reserved_path
                && !tree
                    .root()
                    .metadata()
                    .context
                    .package_sources
                    .as_ref()
                    .is_some_and(|packages| {
                        packages.blocked(vfs, &self.source_path, false).is_some()
                    }),
        )
    }

    pub(crate) fn payload_bytes(&self) -> usize {
        self.name
            .len()
            .saturating_add(self.canonical_source.len())
            .saturating_add(self.source_path.as_os_str().len())
            .saturating_add(self.full_path.len())
            .saturating_add(self.source_transform.as_ref().map_or(0, |transform| {
                transform
                    .original
                    .len()
                    .saturating_add(transform.emitted.len())
            }))
    }
}

/// Eviction only removes authorization metadata, never an editor's durable checkpoint.
/// An evicted or expired ID fails closed and the client must keep its editor draft.
#[derive(Clone, Debug)]
pub(crate) struct RetiredSources {
    entries: std::collections::VecDeque<(std::time::Instant, SourceDescriptor)>,
    bytes: usize,
    pub(crate) max_count: usize,
    pub(crate) max_bytes: usize,
    pub(crate) lifetime: std::time::Duration,
}

impl Default for RetiredSources {
    fn default() -> Self {
        Self {
            entries: Default::default(),
            bytes: 0,
            max_count: MAX_RETIRED_COUNT,
            max_bytes: MAX_RETIRED_BYTES,
            lifetime: RETIRED_LIFETIME,
        }
    }
}

impl RetiredSources {
    pub fn get(&self, id: Ref) -> Option<&SourceDescriptor> {
        self.entries
            .iter()
            .rev()
            .find(|(created, source)| source.id == id && created.elapsed() < self.lifetime)
            .map(|(_, source)| source)
    }

    pub fn retire(&mut self, source: SourceDescriptor) {
        self.forget(source.id);
        let bytes = source.payload_bytes();
        if bytes > self.max_bytes || self.max_count == 0 {
            return;
        }
        while self
            .entries
            .front()
            .is_some_and(|(created, _)| created.elapsed() >= self.lifetime)
            || self.entries.len() >= self.max_count
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some((_, old)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.payload_bytes();
        }
        self.bytes += bytes;
        self.entries.push_back((std::time::Instant::now(), source));
    }

    pub fn forget(&mut self, id: Ref) {
        self.entries.retain(|(_, source)| {
            if source.id == id {
                self.bytes -= source.payload_bytes();
                false
            } else {
                true
            }
        });
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointMetadata {
    pub checkpoint_id: String,
    pub id: Ref,
    pub name: String,
    pub source_path: PathBuf,
    /// Unix epoch milliseconds, informational only, never an authority signal.
    pub created: u64,
    pub provenance: Provenance,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Provenance {
    pub schema_version: u32,
    pub project_identity: PathBuf,
    pub session_id: SessionId,
    pub message_cursor: u32,
    pub captured_cursor: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checkpoint {
    #[serde(flatten)]
    pub metadata: CheckpointMetadata,
    pub source: String,
    pub expected_source: String,
    pub canonical_source: String,
    pub source_transform: Option<SourceTransform>,
}

/// A cross-process lock covers capacity checks and commits. The lock is released by
/// the OS on crashes, unlike a directory lock that could strand all future recovery.
pub(crate) struct Store {
    directory: PathBuf,
    identity: PathBuf,
    _lock: File,
    // Windows denies delete sharing on these handles for the lifetime of every operation.
    // This pins this namespace, not arbitrary ancestors of the project root.
    _directories: Vec<File>,
    max_bytes: u64,
    max_count: usize,
}

pub(crate) fn valid_checkpoint_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id)
        .is_ok_and(|uuid| uuid.get_version_num() == 4 && uuid.to_string() == id)
}

#[cfg(test)]
thread_local! {
    pub(crate) static STORE_OPEN_OBSERVER: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

impl Store {
    #[cfg(test)]
    pub fn open(project_file: &Path) -> anyhow::Result<Self> {
        Self::open_identity(&dunce::canonicalize(project_file)?)
    }

    /// `identity` must be the canonical project path captured by the server session,
    /// never a client-supplied path. Reopening does not consult the live project file.
    pub fn open_identity(identity: &Path) -> anyhow::Result<Self> {
        #[cfg(test)]
        STORE_OPEN_OBSERVER.with_borrow(|observer| {
            if let Some(observer) = observer {
                observer();
            }
        });
        ensure!(
            identity.is_absolute(),
            "Checkpoint project identity must be absolute"
        );
        let identity = identity.to_path_buf();
        let root = identity
            .parent()
            .context("Checkpoint project has no parent")?;
        let cache = root.join(".rojo");
        let mut directories = vec![private_directory(&cache)?];
        let cache = cache.join("source-checkpoints");
        directories.push(private_directory(&cache)?);
        let key = format!(
            "{:x}",
            Sha256::digest(identity.to_string_lossy().as_bytes())
        );
        let directory = cache.join(key);
        directories.push(private_directory(&directory)?);
        let lock_path = directory.join(".lock");
        let lock = match create_private_file(&lock_path, false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                open_private_file(&lock_path, true)?
            }
            Err(error) => return Err(error.into()),
        };
        lock.try_lock().context(
            "Checkpoint storage is busy. Retry after the other recovery operation completes",
        )?;
        let store = Self {
            directory,
            identity,
            _lock: lock,
            _directories: directories,
            max_bytes: MAX_TOTAL_BYTES,
            max_count: MAX_COUNT,
        };
        let owner = store.directory.join("owner.json");
        if owner.exists() {
            let bytes = read_bounded(&owner, 64 * 1024)?;
            let (version, identity): (u32, PathBuf) = serde_json::from_slice(&bytes).context(
                "Checkpoint ownership is unreadable. Preserve this cache and reconcile it manually",
            )?;
            ensure!(version == SCHEMA_VERSION && identity == store.identity,
                "Checkpoint ownership does not match this project. Preserve the cache and reconcile it manually");
        } else {
            store.atomic_write(
                &owner,
                &serde_json::to_vec(&(SCHEMA_VERSION, &store.identity))?,
            )?;
        }
        store.reconcile_incomplete_writes()?;
        Ok(store)
    }

    pub fn create(&self, mut record: Checkpoint) -> anyhow::Result<String> {
        record.metadata.checkpoint_id = uuid::Uuid::new_v4().to_string();
        record.metadata.created = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        ensure!(
            record.metadata.provenance.schema_version == SCHEMA_VERSION
                && record.metadata.provenance.project_identity == self.identity,
            "Checkpoint provenance does not match this project"
        );
        let bytes = serde_json::to_vec(&record)?;
        ensure!(bytes.len() as u64 <= MAX_RECORD_BYTES,
            "Checkpoint exceeds the per-record storage limit. Copy the editor source to a safe file before continuing");
        let mut count = 0;
        let mut total = 0u64;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            let file = open_private_file(&path, false)?;
            total = total
                .checked_add(file.metadata()?.len())
                .context("Checkpoint storage size overflow")?;
            if path.extension().is_some_and(|ext| ext == "json")
                && path.file_name().is_some_and(|name| name != "owner.json")
            {
                count += 1;
            }
        }
        ensure!(count < self.max_count && total.saturating_add(bytes.len() as u64) <= self.max_bytes,
            "Checkpoint storage is full. Read and explicitly discard recovered checkpoints, or copy the editor source to a safe file before continuing. No checkpoints were pruned");
        let id = record.metadata.checkpoint_id.clone();
        self.atomic_write(&self.record_path(&id)?, &bytes)
            .context("Could not persist the checkpoint. Do not replace editor Source until recovery succeeds")?;
        Ok(id)
    }

    pub fn list(&self) -> anyhow::Result<Vec<CheckpointMetadata>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json")
                || path.file_name().is_some_and(|name| name == "owner.json")
            {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("Malformed checkpoint filename")?;
            records.push(self.read(id)?.metadata);
        }
        records.sort_by(|a, b| {
            a.created
                .cmp(&b.created)
                .then_with(|| a.checkpoint_id.cmp(&b.checkpoint_id))
        });
        Ok(records)
    }

    pub fn read(&self, id: &str) -> anyhow::Result<Checkpoint> {
        let path = self.record_path(id)?;
        let record: Checkpoint = serde_json::from_slice(&read_bounded(&path, MAX_RECORD_BYTES)?)
            .context("Checkpoint is unreadable. Preserve its file and recover it manually")?;
        ensure!(record.metadata.checkpoint_id == id
            && record.metadata.provenance.schema_version == SCHEMA_VERSION
            && record.metadata.provenance.project_identity == self.identity,
            "Checkpoint provenance does not match this project or schema. Preserve the file for manual recovery");
        Ok(record)
    }

    pub fn discard(&self, id: &str) -> anyhow::Result<()> {
        // Validate provenance too. Never remove an unknown or foreign cache file.
        self.read(id)?;
        fs::remove_file(self.record_path(id)?)?;
        self.sync_directory()
    }

    fn record_path(&self, id: &str) -> anyhow::Result<PathBuf> {
        ensure!(
            valid_checkpoint_id(id),
            "Malformed checkpoint ID. Use the opaque checkpointId returned by the server"
        );
        Ok(self.directory.join(format!("{id}.json")))
    }

    fn temporary_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix(INCOMPLETE_PREFIX)
            .suffix(INCOMPLETE_SUFFIX)
            .rand_bytes(INCOMPLETE_RANDOM_LENGTH)
            .make_in(&self.directory, |path| create_private_file(path, true))
    }

    fn reconcile_incomplete_writes(&self) -> anyhow::Result<()> {
        let mut removed = false;
        for entry in fs::read_dir(&self.directory)? {
            let path = entry?.path();
            let incomplete = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(INCOMPLETE_PREFIX))
                .and_then(|name| name.strip_suffix(INCOMPLETE_SUFFIX))
                .is_some_and(|token| {
                    token.len() == INCOMPLETE_RANDOM_LENGTH
                        && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
                });
            if incomplete {
                // Only our reserved uncommitted namespace is reclaimable, under the
                // exclusive store lock. UUID.json records are never implicitly removed.
                drop(open_private_file(&path, false).with_context(|| format!(
                    "Cannot safely reclaim incomplete checkpoint file \"{}\". Preserve it and reconcile manually before retrying", path.display()))?);
                fs::remove_file(&path)?;
                removed = true;
            } else {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                ensure!(name == ".lock" || name == "owner.json"
                    || name.strip_suffix(".json").is_some_and(valid_checkpoint_id),
                    "Unrecognized checkpoint storage file \"{}\". Preserve it and reconcile manually before retrying. No checkpoints were pruned", path.display());
            }
        }
        if removed {
            self.sync_directory()?;
        }
        Ok(())
    }

    fn atomic_write(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let mut temporary = self.temporary_file()?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        self.sync_directory()
    }

    fn sync_directory(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        self._directories.last().unwrap().sync_all()?;
        // Windows flushes record contents before an atomic rename, but has no supported
        // directory fsync here. Atomic visibility is not equal power-loss durability.
        #[cfg(not(any(unix, windows)))]
        bail!("Private checkpoint durability is unsupported on this platform");
        #[cfg(any(unix, windows))]
        Ok(())
    }
}

#[cfg(windows)]
fn private_directory(path: &Path) -> anyhow::Result<File> {
    windows::private_directory(path)
}

#[cfg(not(any(unix, windows)))]
fn private_directory(_path: &Path) -> anyhow::Result<File> {
    bail!("Private checkpoint storage is unsupported on this platform")
}

#[cfg(unix)]
fn private_directory(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Checkpoint cache must be an owned real directory, not a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {
                    if let Some(parent) = path.parent() {
                        File::open(parent)?.sync_all()?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return private_directory(path)
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    // Do not chmod the shared .rojo directory, which other server caches also use.
    if path.file_name().is_some_and(|name| name != ".rojo") {
        ensure!(
            fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "Checkpoint directory is not private. Restrict access to its owner before retrying"
        );
    }
    Ok(File::open(path)?)
}

fn open_private_file(path: &Path, writable: bool) -> anyhow::Result<File> {
    #[cfg(windows)]
    return windows::open_file(path, writable).map_err(Into::into);
    #[cfg(not(any(unix, windows)))]
    bail!("Private checkpoint storage is unsupported on this platform");
    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Checkpoint file must be a regular owned file, not a symlink"
        );
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        let opened = file.metadata()?;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(opened.is_file() && metadata.dev() == opened.dev() && metadata.ino() == opened.ino()
            && opened.permissions().mode() & 0o077 == 0 && opened.nlink() == 1,
            "Checkpoint file is not private or has additional hard links. Preserve it and restrict access before retrying");
        Ok(file)
    }
}

fn create_private_file(path: &Path, temporary: bool) -> std::io::Result<File> {
    #[cfg(windows)]
    return windows::create_file(path, temporary);
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::Error::other(
        "Private checkpoint storage is unsupported on this platform",
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = temporary;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
}

fn read_bounded(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_private_file(path, false)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Checkpoint file exceeds its size limit. Preserve it for manual recovery");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn project() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("default.project.json");
        fs::write(&project, "{}").unwrap();
        (directory, project)
    }

    pub(super) fn record(store: &Store) -> Checkpoint {
        Checkpoint {
            metadata: CheckpointMetadata {
                checkpoint_id: String::new(),
                id: Ref::new(),
                name: "Editor".into(),
                source_path: store.identity.parent().unwrap().join("Editor.luau"),
                created: 0,
                provenance: Provenance {
                    schema_version: 1,
                    project_identity: store.identity.clone(),
                    session_id: SessionId::new(),
                    message_cursor: 1,
                    captured_cursor: 2,
                },
            },
            source: "editor source\n".into(),
            expected_source: "expected source\n".into(),
            canonical_source: "filesystem source\n".into(),
            source_transform: Some(SourceTransform {
                original: "authored".into(),
                emitted: "transformed".into(),
            }),
        }
    }

    #[test]
    fn atomic_checkpoint_survives_restart_list_read_and_explicit_discard() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let id = store.create(record(&store)).unwrap();
        assert!(valid_checkpoint_id(&id));
        assert_eq!(fs::read_dir(&store.directory).unwrap().count(), 3);
        assert!(
            Store::open(&project).is_err(),
            "concurrent stores must serialize capacity checks"
        );
        drop(store);
        let store = Store::open(&project).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].checkpoint_id, id);
        let recovered = store.read(&id).unwrap();
        assert_eq!(recovered.source, "editor source\n");
        assert_eq!(recovered.expected_source, "expected source\n");
        assert_eq!(recovered.canonical_source, "filesystem source\n");
        assert_eq!(recovered.source_transform.unwrap().original, "authored");
        store.discard(&id).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.read(&id).is_err());
    }

    #[test]
    fn failed_atomic_commit_preserves_existing_record_and_cleans_temporary_file() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let id = store.create(record(&store)).unwrap();
        let path = store.record_path(&id).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(store.atomic_write(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&store.directory).unwrap().count(), 3);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn abandoned_temporary_write_is_reclaimed_on_restart_without_pruning_records() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let identity = store.identity.clone();
        let id = store.create(record(&store)).unwrap();
        let committed = fs::read(store.record_path(&id).unwrap()).unwrap();
        let temporary = store.temporary_file().unwrap();
        temporary.as_file().set_len(MAX_TOTAL_BYTES).unwrap();
        let (file, abandoned) = temporary.keep().unwrap();
        drop(file);
        assert!(store.create(record(&store)).is_err());
        assert!(abandoned.exists());
        drop(store);
        let store = Store::open_identity(&identity).unwrap();
        assert!(!abandoned.exists());
        assert_eq!(
            fs::read(store.record_path(&id).unwrap()).unwrap(),
            committed
        );
        store.create(record(&store)).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn restart_reconciliation_preserves_unrecognized_files_and_malformed_commits() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let identity = store.identity.clone();
        let unknown = store.directory.join(".unrecognized-temporary");
        let malformed = store
            .record_path(&uuid::Uuid::new_v4().to_string())
            .unwrap();
        store.atomic_write(&unknown, b"unknown").unwrap();
        store.atomic_write(&malformed, b"{broken").unwrap();
        drop(store);
        let error = Store::open_identity(&identity).err().unwrap();
        assert!(error
            .to_string()
            .contains("Unrecognized checkpoint storage file"));
        assert_eq!(fs::read(&unknown).unwrap(), b"unknown");
        assert_eq!(fs::read(&malformed).unwrap(), b"{broken");
        // Only an explicit manual reconciliation removes unrecognized legacy bytes.
        fs::remove_file(unknown).unwrap();
        let store = Store::open_identity(&identity).unwrap();
        assert!(store.list().is_err());
        assert!(malformed.exists());
    }

    #[test]
    fn retained_identity_allows_list_read_and_discard_after_project_deletion() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let identity = store.identity.clone();
        let id = store.create(record(&store)).unwrap();
        drop(store);
        fs::remove_file(project).unwrap();
        let store = Store::open_identity(&identity).unwrap();
        assert_eq!(store.list().unwrap()[0].checkpoint_id, id);
        assert_eq!(store.read(&id).unwrap().source, "editor source\n");
        store.discard(&id).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn project_identity_partitions_projects_sharing_a_root() {
        let (directory, project) = project();
        let second = directory.path().join("second.project.json");
        fs::write(&second, "{}").unwrap();
        let first = Store::open(&project).unwrap();
        let second = Store::open(&second).unwrap();
        let id = first.create(record(&first)).unwrap();
        assert!(second.list().unwrap().is_empty());
        assert!(second.read(&id).is_err());
        assert!(second.discard(&id).is_err());
        assert_eq!(first.list().unwrap().len(), 1);
    }

    #[test]
    fn capacity_failures_never_prune_unresolved_records() {
        let (_directory, project) = project();
        let mut store = Store::open(&project).unwrap();
        store.max_count = 1;
        let id = store.create(record(&store)).unwrap();
        assert!(store
            .create(record(&store))
            .unwrap_err()
            .to_string()
            .contains("No checkpoints were pruned"));
        assert_eq!(store.read(&id).unwrap().source, "editor source\n");
        store.max_count = 2;
        store.max_bytes = 1;
        assert!(store.create(record(&store)).is_err());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_traversal_foreign_identity_and_incomplete_atomic_payloads() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        for id in [
            "../owner",
            "owner",
            "/tmp/escape",
            "..%2fescape",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(store.read(id).is_err());
            assert!(store.discard(id).is_err());
        }
        let mut foreign = record(&store);
        foreign.metadata.provenance.project_identity = PathBuf::from("foreign.project.json");
        assert!(store.create(foreign).is_err());
        let temporary = store.temporary_file().unwrap();
        assert!(store.list().unwrap().is_empty());
        drop(temporary);
        let id = uuid::Uuid::new_v4().to_string();
        store
            .atomic_write(&store.record_path(&id).unwrap(), b"{broken")
            .unwrap();
        assert!(store.list().is_err());
        assert!(store.discard(&id).is_err());
        assert!(store.record_path(&id).unwrap().exists());
    }

    #[test]
    fn owner_schema_failure_preserves_committed_and_incomplete_records() {
        for (version, foreign) in [(SCHEMA_VERSION + 1, false), (SCHEMA_VERSION, true)] {
            let (_directory, project) = project();
            let store = Store::open(&project).unwrap();
            let identity = store.identity.clone();
            let id = store.create(record(&store)).unwrap();
            let record_path = store.record_path(&id).unwrap();
            let owner = store.directory.join("owner.json");
            let (file, abandoned) = store.temporary_file().unwrap().keep().unwrap();
            drop(file);
            let claimed = if foreign {
                identity.with_file_name("foreign.project.json")
            } else {
                identity.clone()
            };
            fs::write(owner, serde_json::to_vec(&(version, claimed)).unwrap()).unwrap();
            drop(store);
            assert!(Store::open_identity(&identity).is_err());
            assert!(record_path.exists());
            assert!(
                abandoned.exists(),
                "Ownership must be checked before reclamation"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissive_parent_is_unchanged_and_insecure_records_are_never_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, project) = project();
        let cache = directory.path().join(".rojo");
        fs::create_dir(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::open(&project).unwrap();
        assert_eq!(
            fs::metadata(cache).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let id = store.create(record(&store)).unwrap();
        let path = store.record_path(&id).unwrap();
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.read(&id).is_err());
        assert!(store.discard(&id).is_err());
        assert!(store.create(record(&store)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = store.directory.join("linked");
        fs::hard_link(&path, &link).unwrap();
        assert!(store.read(&id).is_err());
        assert!(store.discard(&id).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_cache_and_record_files() {
        let (directory, project) = project();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join(".rojo")).unwrap();
        assert!(Store::open(&project).is_err());
        fs::remove_file(directory.path().join(".rojo")).unwrap();
        let store = Store::open(&project).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        std::os::unix::fs::symlink(&project, store.record_path(&id).unwrap()).unwrap();
        assert!(store.read(&id).is_err());
        assert!(store.discard(&id).is_err());
        assert!(store.create(record(&store)).is_err());
    }
}
