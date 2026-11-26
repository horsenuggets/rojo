use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet},
    io,
};

use crossbeam_channel::Receiver;
use notify::{raw_watcher, RawEvent, RecommendedWatcher, RecursiveMode, Watcher};

use crate::watch_batch::{exists_exactly, WatchBatch};
use crate::{DirEntry, FileIdentity, IoResultExt, Metadata, ReadDir, VfsBackend, VfsEvent};

/// A batch settles once events stop arriving for this long, which gathers the
/// separate events of one save or move.
const BATCH_QUIET: Duration = Duration::from_millis(50);

/// A steady stream of events still settles this often, so a long build that
/// keeps writing files can't hold every change back until it finishes.
const BATCH_LIMIT: Duration = Duration::from_millis(500);

/// `VfsBackend` that uses `std::fs` and the `notify` crate.
pub struct StdBackend {
    watcher: Mutex<RecommendedWatcher>,
    watcher_receiver: Receiver<VfsEvent>,
    watches: HashSet<PathBuf>,
    identities: Arc<Mutex<HashMap<PathBuf, FileIdentity>>>,
}

impl StdBackend {
    pub fn new() -> io::Result<StdBackend> {
        let (notify_tx, notify_rx) = mpsc::channel();
        // The raw watcher, because notify's debouncer trusts the event kinds,
        // which macOS reports wrongly. See `watch_batch`.
        let watcher = raw_watcher(notify_tx).map_err(io::Error::other)?;

        let (tx, rx) = crossbeam_channel::unbounded();
        let identities = Arc::new(Mutex::new(HashMap::new()));
        let watched_identities = identities.clone();

        thread::spawn(move || {
            while let Ok(first) = notify_rx.recv() {
                let mut batch = WatchBatch::default();
                record(&mut batch, first);

                let started = Instant::now();
                let mut disconnected = false;
                while started.elapsed() < BATCH_LIMIT {
                    match notify_rx.recv_timeout(BATCH_QUIET) {
                        Ok(event) => record(&mut batch, event),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                let mut entries = HashMap::new();
                let events = batch.settle(|path| exists_exactly(path, &mut entries));
                #[cfg(windows)]
                let events = filter_windows_directory_metadata_events(events);
                let mut known = watched_identities.lock().unwrap();
                let mut current = HashMap::new();
                for event in &events {
                    let path = match event {
                        VfsEvent::Rename { to, .. } => to,
                        VfsEvent::Create(path) | VfsEvent::Write(path) => path,
                        _ => continue,
                    };
                    if let Some(identity) = observed_identity(path) {
                        current.insert(path.clone(), identity);
                    }
                }
                let events = link_identity_renames(events, &known, &current);
                let mut events =
                    link_moves_into_created_directories(events, &known, &mut current, |path| {
                        identities_under(path)
                    });
                for event in &mut events {
                    if let VfsEvent::Rename { to, identity, .. } = event {
                        *identity = current.get(to).copied();
                    }
                }
                update_known_identities(&mut known, &events, current);
                drop(known);
                if !send_batch(&tx, events) {
                    return;
                }
                if disconnected {
                    return;
                }
            }
        });

        Ok(Self {
            watcher: Mutex::new(watcher),
            watcher_receiver: rx,
            watches: HashSet::new(),
            identities,
        })
    }
}

impl VfsBackend for StdBackend {
    fn lock_exclusive(&self, path: &Path) -> io::Result<crate::VfsLockGuard> {
        use std::fs::OpenOptions;

        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "A lock must have a parent directory",
            )
        })?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        // The coordinator keeps this file outside the journal cache. Its parent may
        // need creating on first use, but it must never be a link or a file.
        match std::fs::create_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        if !parent_metadata.is_dir() {
            return Err(io::Error::other("A lock parent must be a real directory"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if parent_metadata.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("A lock parent cannot be a reparse point"));
            }
        }
        if let Some(metadata) = std::fs::symlink_metadata(path).with_not_found()? {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(io::Error::other("A lock must be a regular file"));
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        // NOFOLLOW closes the final-component check/open race. NONBLOCK avoids
        // hanging on a FIFO substituted before open. No data is ever written.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(target_os = "linux")]
            options.custom_flags(0x20000 | 0x800);
            #[cfg(target_os = "macos")]
            options.custom_flags(0x100 | 0x4);
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unsupported lock platform",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open the reparse point itself and prevent deletion/replacement while
            // any holder or waiter has the file open.
            options.custom_flags(0x00200000).share_mode(0x1 | 0x2);
        }
        let file = options.open(path)?;
        validate_lock_file(&file, path)?;
        file.lock()?;
        // A waiter may have opened the old inode before a replacement. Never
        // proceed with exclusion on a file no longer named by the lock path.
        validate_lock_file(&file, path)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            std::fs::File::open(parent)?.sync_all()?;
            if let Some(grandparent) = parent.parent() {
                let grandparent = if grandparent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    grandparent
                };
                std::fs::File::open(grandparent)?.sync_all()?;
            }
        }
        Ok(crate::VfsLockGuard { _file: Some(file) })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs_err::read(path)
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        fs_err::write(path, data)
    }

    fn write_atomic(
        &mut self,
        path: &Path,
        data: &[u8],
        expected: Option<&[u8]>,
    ) -> io::Result<()> {
        use std::io::Write;

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("A file must have a parent directory"))?;
        let metadata = fs_err::symlink_metadata(path).with_not_found()?;
        if metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_file())
        {
            return Err(io::Error::other("Atomic updates require a regular file"));
        }
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(data)?;
        if let Some(metadata) = metadata {
            staged.as_file().set_permissions(metadata.permissions())?;
        }
        staged.as_file().sync_all()?;
        if self.read(path).with_not_found()?.as_deref() != expected {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Filesystem changed before persistence",
            ));
        }
        if expected.is_some() {
            staged.persist(path).map_err(|error| error.error)?;
        } else {
            staged
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
        }
        Ok(())
    }

    fn snapshot_tree(&self, path: &Path) -> io::Result<crate::VfsSnapshot> {
        let metadata = fs_err::symlink_metadata(path)?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Relocation rejects links and special files",
            ));
        }
        crate::relocation::snapshot_tree(self, path)
    }

    fn move_no_replace(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &crate::VfsSnapshot,
    ) -> io::Result<()> {
        if &self.snapshot_tree(source)? != expected {
            return Err(crate::relocation::changed());
        }
        crate::relocation::rename_no_replace(source, destination)
    }

    fn copy_permissions(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        let source_metadata = fs_err::symlink_metadata(source)?;
        let destination_metadata = fs_err::symlink_metadata(destination)?;
        if !(source_metadata.is_file() && destination_metadata.is_file()
            || source_metadata.is_dir() && destination_metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Permission copies require matching regular entries",
            ));
        }
        fs_err::set_permissions(destination, source_metadata.permissions())
    }

    fn sync_path(&mut self, path: &Path) -> io::Result<()> {
        let metadata = fs_err::symlink_metadata(path)?;
        if metadata.is_file() {
            #[cfg(windows)]
            return std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all();
            #[cfg(not(windows))]
            return std::fs::File::open(path)?.sync_all();
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Cannot flush a link or special file",
            ));
        }
        #[cfg(unix)]
        {
            std::fs::File::open(path)?.sync_all()
        }
        // Windows has no portable directory fsync. Moves use WRITE_THROUGH and
        // journal files use FlushFileBuffers, but power-loss ordering of newly
        // created directory entries is not promised there.
        #[cfg(windows)]
        {
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Cannot flush directories on this platform",
            ))
        }
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        std::fs::exists(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        let entries: Result<Vec<_>, _> = fs_err::read_dir(path)?.collect();
        let mut entries = entries?;

        entries.sort_by_cached_key(|entry| entry.file_name());

        let inner = entries
            .into_iter()
            .map(|entry| Ok(DirEntry { path: entry.path() }));

        Ok(ReadDir {
            inner: Box::new(inner),
        })
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir(path)
    }

    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir_all(path)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_file(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_dir_all(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let inner = fs_err::metadata(path)?;
        #[cfg(not(windows))]
        let identity = file_identity(&inner);
        #[cfg(windows)]
        let identity = observed_identity(path);
        if let Some(identity) = identity {
            self.identities
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), identity);
        }

        Ok(Metadata {
            is_file: inner.is_file(),
        })
    }

    fn file_identity(&self, path: &Path) -> io::Result<Option<FileIdentity>> {
        Ok(observed_identity(path))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        dunce::canonicalize(path)
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.watcher_receiver.clone()
    }

    fn watch(&mut self, path: &Path) -> io::Result<()> {
        if self.watches.contains(path)
            || path
                .ancestors()
                .any(|ancestor| self.watches.contains(ancestor))
        {
            Ok(())
        } else {
            self.watcher
                .get_mut()
                .unwrap()
                .watch(path, RecursiveMode::Recursive)
                .map_err(io::Error::other)?;
            self.watches.insert(path.to_path_buf());
            Ok(())
        }
    }

    fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        // Unknown paths can restart the native stream even when unwatch returns an error.
        if !self.watches.remove(path) {
            return Ok(());
        }
        self.watcher
            .get_mut()
            .unwrap()
            .unwatch(path)
            .map_err(io::Error::other)
    }
}

#[cfg(not(windows))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A stationary hard-link alias can look like a move when another alias is
        // deleted and recreated. Directory link counts do not imply such aliases.
        if _metadata.is_file() && _metadata.nlink() != 1 {
            return None;
        }
        Some(FileIdentity {
            device: _metadata.dev(),
            inode: _metadata.ino(),
            // Birth time prevents a deleted inode reused for a new file from matching.
            // Filesystems without it fall back to native watcher linkage only.
            created: _metadata.created().ok()?,
        })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn observed_identity(_path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        file_identity(&std::fs::metadata(_path).ok()?)
    }
    #[cfg(windows)]
    {
        // Consumer validation must reopen the path, never use the watcher's cache.
        windows_identity(_path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(windows)]
fn windows_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE)
        // Do not use OPEN_REPARSE_POINT. Like std metadata, follow symlinks and junctions.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // The handle stays open for the call and the API initializes every field on success.
    let info = unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0 {
            return None;
        }
        info.assume_init()
    };
    windows_identity_from_info(&info)
}

#[cfg(windows)]
fn windows_identity_from_info(
    info: &windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> Option<FileIdentity> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    // Hard-link aliases are ambiguous, but directory link counts are not aliases.
    if !is_directory && info.nNumberOfLinks != 1 {
        return None;
    }
    let inode = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    // Unsupported or unrepresentable 64-bit file IDs cannot prove rename identity.
    if inode == 0 || inode == u64::MAX {
        return None;
    }
    let created = (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
        | u64::from(info.ftCreationTime.dwLowDateTime);
    let elapsed = created.checked_sub(116_444_736_000_000_000)?;
    Some(FileIdentity {
        device: u64::from(info.dwVolumeSerialNumber),
        inode,
        created: std::time::UNIX_EPOCH
            + Duration::new(elapsed / 10_000_000, ((elapsed % 10_000_000) * 100) as u32),
    })
}

#[cfg(windows)]
fn filter_windows_directory_metadata_events(events: Vec<VfsEvent>) -> Vec<VfsEvent> {
    // ReadDirectoryChangesW reports directory timestamp/attribute updates separately
    // from child name and file writes. They have no serialized content of their own.
    // Resnapshotting an ancestor for those echoes can race the next linked move and
    // recreate its subtree before that move's events arrive.
    events
        .into_iter()
        .filter(|event| {
            !matches!(event, VfsEvent::Write(path)
                if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        })
        .collect()
}

/// A native watcher may omit a rename cookie. Only pair those endpoints when an earlier
/// observation and the new file have the same filesystem identity, never by content.
fn link_identity_renames(
    events: Vec<VfsEvent>,
    before: &HashMap<PathBuf, FileIdentity>,
    after: &HashMap<PathBuf, FileIdentity>,
) -> Vec<VfsEvent> {
    let mut validated = Vec::with_capacity(events.len());
    for event in events {
        match event {
            VfsEvent::Rename { from, to, .. }
                if before
                    .get(&from)
                    .zip(after.get(&to))
                    .is_some_and(|(old, new)| old != new) =>
            {
                validated.push(VfsEvent::Remove(from));
                validated.push(VfsEvent::Create(to));
            }
            event => validated.push(event),
        }
    }
    let events = validated;
    let mut removals: HashMap<FileIdentity, Vec<usize>> = HashMap::new();
    let mut creations: HashMap<FileIdentity, Vec<usize>> = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            VfsEvent::Remove(path) => {
                if let Some(identity) = before.get(path) {
                    removals.entry(*identity).or_default().push(index);
                }
            }
            VfsEvent::Create(path) => {
                if let Some(identity) = after.get(path) {
                    creations.entry(*identity).or_default().push(index);
                }
            }
            _ => {}
        }
    }
    let mut replacements = HashMap::new();
    let mut skipped = vec![false; events.len()];
    for (identity, removed) in removals {
        let [a] = removed.as_slice() else { continue };
        let Some(created) = creations.get(&identity) else {
            continue;
        };
        let [b] = created.as_slice() else { continue };
        let (VfsEvent::Remove(from), VfsEvent::Create(to)) = (&events[*a], &events[*b]) else {
            unreachable!();
        };
        replacements.insert(
            (*a).min(*b),
            VfsEvent::Rename {
                from: from.clone(),
                to: to.clone(),
                identity: None,
            },
        );
        skipped[(*a).max(*b)] = true;
    }
    events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| {
            if skipped[index] {
                None
            } else {
                Some(replacements.remove(&index).unwrap_or(event))
            }
        })
        .collect()
}

/// inotify watches directories one at a time. When something moves into a directory
/// created a moment earlier, its watch may not exist yet, so only the move-out is
/// reported. The moved object must then be inside a directory this batch created. Link
/// it there only when exactly one entry carries its earlier filesystem identity.
fn link_moves_into_created_directories(
    events: Vec<VfsEvent>,
    before: &HashMap<PathBuf, FileIdentity>,
    after: &mut HashMap<PathBuf, FileIdentity>,
    mut identities_under: impl FnMut(&Path) -> Vec<(PathBuf, FileIdentity)>,
) -> Vec<VfsEvent> {
    let wanted: HashSet<FileIdentity> = events
        .iter()
        .filter_map(|event| match event {
            VfsEvent::Remove(path) => before.get(path).copied(),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return events;
    }
    let created: Vec<&Path> = events
        .iter()
        .filter_map(|event| match event {
            VfsEvent::Create(path) => Some(path.as_path()),
            _ => None,
        })
        .collect();
    // Nested creations are already inside an outer created directory's walk.
    let roots = created.iter().filter(|path| {
        !path
            .ancestors()
            .skip(1)
            .any(|ancestor| created.contains(&ancestor))
    });
    let mut found: HashMap<FileIdentity, Vec<PathBuf>> = HashMap::new();
    for root in roots {
        for (path, identity) in identities_under(root) {
            if wanted.contains(&identity) {
                found.entry(identity).or_default().push(path);
            }
        }
    }
    let reported: HashSet<PathBuf> = events
        .iter()
        .filter_map(|event| match event {
            VfsEvent::Rename { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect();
    events
        .into_iter()
        .map(|event| {
            let VfsEvent::Remove(from) = &event else {
                return event;
            };
            let Some(identity) = before.get(from) else {
                return event;
            };
            match found.get(identity).map(Vec::as_slice) {
                Some([to]) if !reported.contains(to) => {
                    after.insert(to.clone(), *identity);
                    VfsEvent::Rename {
                        from: from.clone(),
                        to: to.clone(),
                        identity: None,
                    }
                }
                _ => event,
            }
        })
        .collect()
}

/// Lists the identities of every entry beneath a newly created directory, without
/// following links into other trees.
fn identities_under(root: &Path) -> Vec<(PathBuf, FileIdentity)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_symlink() {
                continue;
            }
            if let Some(identity) = observed_identity(&path) {
                found.push((path.clone(), identity));
            }
            if metadata.is_dir() {
                pending.push(path);
            }
        }
    }
    found
}

fn update_known_identities(
    known: &mut HashMap<PathBuf, FileIdentity>,
    events: &[VfsEvent],
    current: HashMap<PathBuf, FileIdentity>,
) {
    let mut removed = HashSet::new();
    let mut renames = HashMap::new();
    for event in events {
        match event {
            VfsEvent::Remove(path) => {
                removed.insert(path.as_path());
            }
            VfsEvent::Rename { from, to, .. } => {
                removed.insert(from.as_path());
                renames.insert(from.as_path(), to.as_path());
            }
            _ => {}
        }
    }
    let mut relocated = Vec::new();
    if !removed.is_empty() {
        known.retain(|path, identity| {
            if let Some((from, to)) = path
                .ancestors()
                .find_map(|from| renames.get(from).map(|to| (from, *to)))
            {
                relocated.push((to.join(path.strip_prefix(from).unwrap()), *identity));
                false
            } else {
                !path.ancestors().any(|path| removed.contains(path))
            }
        });
    }
    // A watcher may settle another burst before the consumer snapshots the new
    // directory. Carry its descendants too, so that burst can still identify them.
    known.extend(relocated);
    known.extend(current);
}

fn send_batch(sender: &crossbeam_channel::Sender<VfsEvent>, events: Vec<VfsEvent>) -> bool {
    events.is_empty() || sender.send(VfsEvent::Batch(events)).is_ok()
}

/// Adds a raw event to the batch. Events without a path, like the rescan
/// request FSEvents sends after dropping events, were ignored by the debouncer
/// this replaces too.
fn record(batch: &mut WatchBatch, event: RawEvent) {
    if let (Some(path), Ok(op)) = (event.path, event.op) {
        batch.record(path, op, event.cookie);
    }
}

fn validate_lock_file(file: &std::fs::File, path: &Path) -> io::Result<()> {
    let opened = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    if !opened.is_file() || !named.is_file() || named.file_type().is_symlink() {
        return Err(io::Error::other("A lock must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 || opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(io::Error::other("The lock file was linked or replaced"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened.file_attributes() & 0x400 != 0 || named.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("A lock cannot be a reparse point"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(unix, windows))]
    #[test]
    fn identity_validation_freshly_observes_replaced_files_and_directories() {
        let root = tempfile::tempdir().unwrap();
        let backend = StdBackend::new().unwrap();
        for is_file in [false, true] {
            let path = root.path().join(if is_file { "file" } else { "directory" });
            let replacement = path.with_extension("replacement");
            let retained = path.with_extension("retained");
            for entry in [&path, &replacement] {
                if is_file {
                    std::fs::write(entry, "same contents").unwrap();
                } else {
                    std::fs::create_dir(entry).unwrap();
                }
            }
            assert_eq!(backend.metadata(&path).unwrap().is_file(), is_file);
            let old = backend.file_identity(&path).unwrap().unwrap();
            let new = backend.file_identity(&replacement).unwrap().unwrap();
            assert_ne!(old, new);
            std::fs::rename(&path, &retained).unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            // No watcher is registered, so the earlier observation is still cached.
            assert_eq!(backend.identities.lock().unwrap().get(&path), Some(&old));
            let observed = backend.file_identity(&path).unwrap().unwrap();
            assert_ne!(observed, old);
            assert_eq!(observed.device, new.device);
            assert_eq!(observed.inode, new.inode);
            // NTFS tunneling can restore the old name's creation time on replacement.
            assert_eq!(
                observed.created,
                std::fs::metadata(&path).unwrap().created().unwrap()
            );
            assert_eq!(observed_identity(&path), Some(observed));
            assert_eq!(backend.file_identity(&replacement).unwrap(), None);
            assert_eq!(backend.metadata(&path).unwrap().is_file(), is_file);
            assert_eq!(
                backend.identities.lock().unwrap().get(&path),
                Some(&observed)
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_matches_creation_time_and_token_eligibility() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("directory");
        let file = root.path().join("file");
        let alias = root.path().join("alias");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(directory.join("child")).unwrap();
        std::fs::write(&file, "contents").unwrap();
        let backend = StdBackend::new().unwrap();
        for path in [&file, &directory] {
            let standard = std::fs::metadata(path).unwrap();
            let identity = windows_identity(path);
            assert_eq!(identity.unwrap().created, standard.created().unwrap());
            assert_eq!(
                backend.metadata(path).unwrap().is_file(),
                standard.is_file()
            );
            assert_eq!(backend.file_identity(path).unwrap(), identity);
        }
        std::fs::hard_link(&file, &alias).unwrap();
        for path in [&file, &alias] {
            assert_eq!(windows_identity(path), None);
            assert!(backend.metadata(path).unwrap().is_file());
            assert_eq!(backend.file_identity(path).unwrap(), None);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_handle_info_preserves_identity_fields_and_link_eligibility() {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        };
        let elapsed = 12_345_678;
        let created = 116_444_736_000_000_000u64 + elapsed;
        for directory in [false, true] {
            for links in [0, 1, 2, 7] {
                for valid_birth in [false, true] {
                    let birth = if valid_birth { created } else { 0 };
                    let info = BY_HANDLE_FILE_INFORMATION {
                        dwFileAttributes: if directory {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            0
                        },
                        dwVolumeSerialNumber: 23,
                        ftCreationTime: FILETIME {
                            dwHighDateTime: (birth >> 32) as u32,
                            dwLowDateTime: birth as u32,
                        },
                        nFileIndexHigh: 45,
                        nFileIndexLow: 67,
                        nNumberOfLinks: links,
                        ..Default::default()
                    };
                    let expected =
                        (valid_birth && (directory || links == 1)).then_some(FileIdentity {
                            device: 23,
                            inode: (45u64 << 32) | 67,
                            created: std::time::UNIX_EPOCH + Duration::new(1, 234_567_800),
                        });
                    assert_eq!(windows_identity_from_info(&info), expected);
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_handle_info_rejects_unsupported_file_ids_but_accepts_zero_volume() {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        };
        let created = 116_444_736_000_000_001u64;
        for directory in [false, true] {
            for device in [0, 23] {
                for inode in [0, 1, (45u64 << 32) | 67, u64::MAX - 1, u64::MAX] {
                    let info = BY_HANDLE_FILE_INFORMATION {
                        dwFileAttributes: if directory {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            0
                        },
                        dwVolumeSerialNumber: device,
                        ftCreationTime: FILETIME {
                            dwHighDateTime: (created >> 32) as u32,
                            dwLowDateTime: created as u32,
                        },
                        nFileIndexHigh: (inode >> 32) as u32,
                        nFileIndexLow: inode as u32,
                        nNumberOfLinks: 1,
                        ..Default::default()
                    };
                    let expected = (inode != 0 && inode != u64::MAX).then_some(FileIdentity {
                        device: u64::from(device),
                        inode,
                        created: std::time::UNIX_EPOCH + Duration::from_nanos(100),
                    });
                    assert_eq!(windows_identity_from_info(&info), expected);
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_follows_symlink_targets() {
        let root = tempfile::tempdir().unwrap();
        let backend = StdBackend::new().unwrap();
        for is_file in [false, true] {
            let target = root.path().join(if is_file { "file" } else { "directory" });
            let link = target.with_extension("link");
            let result = if is_file {
                std::fs::write(&target, "contents").unwrap();
                std::os::windows::fs::symlink_file(&target, &link)
            } else {
                std::fs::create_dir(&target).unwrap();
                std::os::windows::fs::symlink_dir(&target, &link)
            };
            if let Err(error) = result {
                if error.raw_os_error() == Some(1314) || error.kind() == io::ErrorKind::Unsupported
                {
                    eprintln!("Skipping symlink observation because symlink creation is unavailable ({error}).");
                    return;
                }
                panic!("Could not create test symlink ({}).", error);
            }
            let metadata = backend.metadata(&link).unwrap();
            let identity = windows_identity(&link);
            assert_eq!(
                metadata.is_file(),
                std::fs::metadata(&link).unwrap().is_file()
            );
            assert_eq!(metadata.is_file(), is_file);
            assert_eq!(identity, windows_identity(&target));
            assert!(identity.is_some());
            assert_eq!(backend.file_identity(&link).unwrap(), identity);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_follows_junction_targets() {
        let root = tempfile::tempdir().unwrap();
        let backend = StdBackend::new().unwrap();
        let target = root.path().join("directory");
        let junction = root.path().join("junction");
        std::fs::create_dir(&target).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        if !output.status.success() {
            eprintln!(
                "Skipping junction observation because junction creation is unavailable ({}).",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let metadata = backend.metadata(&junction).unwrap();
        let identity = windows_identity(&junction);
        assert_eq!(
            metadata.is_file(),
            std::fs::metadata(&junction).unwrap().is_file()
        );
        assert!(!metadata.is_file());
        assert_eq!(identity, windows_identity(&target));
        assert!(identity.is_some());
        assert_eq!(backend.file_identity(&junction).unwrap(), identity);
        std::fs::remove_dir(&junction).unwrap();
        assert!(target.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn directory_metadata_echoes_keep_file_writes_and_all_structural_events() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("directory");
        let file = root.path().join("source.luau");
        let missing = root.path().join("missing");
        let destination = root.path().join("destination");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&file, "return 1\n").unwrap();
        let events = filter_windows_directory_metadata_events(vec![
            VfsEvent::Write(directory.clone()),
            VfsEvent::Write(file.clone()),
            VfsEvent::Write(missing.clone()),
            VfsEvent::Create(directory.clone()),
            VfsEvent::Remove(directory.clone()),
            VfsEvent::Rename {
                from: directory.clone(),
                to: destination.clone(),
                identity: None,
            },
        ]);
        assert_eq!(events.len(), 5);
        assert!(matches!(&events[0], VfsEvent::Write(path) if path == &file));
        assert!(matches!(&events[1], VfsEvent::Write(path) if path == &missing));
        assert!(matches!(&events[2], VfsEvent::Create(path) if path == &directory));
        assert!(matches!(&events[3], VfsEvent::Remove(path) if path == &directory));
        assert!(matches!(&events[4], VfsEvent::Rename { from, to, .. }
            if from == &directory && to == &destination));
    }

    #[test]
    fn identity_linkage_requires_a_unique_unchanged_file_identity() {
        let identity = FileIdentity {
            device: 1,
            inode: 2,
            created: std::time::UNIX_EPOCH,
        };
        for same_inode in [false, true] {
            for same_birth in [false, true] {
                for ambiguous in [false, true] {
                    let from = PathBuf::from("old");
                    let to = PathBuf::from("new");
                    let mut before = HashMap::from([(from.clone(), identity)]);
                    let after = HashMap::from([(
                        to.clone(),
                        FileIdentity {
                            inode: if same_inode { 2 } else { 3 },
                            created: identity.created + Duration::from_secs(u64::from(!same_birth)),
                            ..identity
                        },
                    )]);
                    let mut events = vec![VfsEvent::Remove(from), VfsEvent::Create(to)];
                    if ambiguous {
                        let other = PathBuf::from("other");
                        before.insert(other.clone(), identity);
                        events.push(VfsEvent::Remove(other));
                    }
                    let linked = link_identity_renames(events, &before, &after);
                    assert_eq!(
                        linked
                            .iter()
                            .any(|event| matches!(event, VfsEvent::Rename { .. })),
                        same_inode && same_birth && !ambiguous
                    );
                }
            }
        }
    }

    #[test]
    fn moves_into_created_directories_link_only_a_unique_known_identity() {
        let moved = FileIdentity {
            device: 1,
            inode: 2,
            created: std::time::UNIX_EPOCH,
        };
        let other = FileIdentity { inode: 3, ..moved };
        let from = PathBuf::from("/src/Module.luau");
        let created = PathBuf::from("/src/New");
        let destination = PathBuf::from("/src/New/Deep/Module.luau");
        // Each row is whether the identity was known, whether a matching entry exists
        // in the created directory, whether a second entry shares it, and whether the
        // batch created a directory at all.
        for row in 0..16u8 {
            let known = row & 1 != 0;
            let present = row & 2 != 0;
            let duplicated = row & 4 != 0;
            let creates = row & 8 != 0;
            let before = if known {
                HashMap::from([(from.clone(), moved)])
            } else {
                HashMap::new()
            };
            let mut events = vec![VfsEvent::Remove(from.clone())];
            if creates {
                events.insert(0, VfsEvent::Create(created.clone()));
            }
            let mut after = HashMap::new();
            let mut walked = Vec::new();
            let linked = link_moves_into_created_directories(events, &before, &mut after, |root| {
                walked.push(root.to_path_buf());
                let mut entries = vec![(created.join("Unrelated.luau"), other)];
                if present {
                    entries.push((destination.clone(), moved));
                }
                if present && duplicated {
                    entries.push((created.join("Copy.luau"), moved));
                }
                entries
            });
            let expected = known && present && !duplicated && creates;
            let renamed = linked.iter().any(|event| {
                matches!(event, VfsEvent::Rename { from: old, to: new, .. }
                    if old == &from && new == &destination)
            });
            assert_eq!(renamed, expected, "Row {row} linked incorrectly");
            assert_eq!(
                linked
                    .iter()
                    .any(|event| matches!(event, VfsEvent::Remove(path) if path == &from)),
                !expected
            );
            assert_eq!(after.get(&destination).copied(), expected.then_some(moved));
            // Walking only happens when a removal could be linked into a new directory.
            assert_eq!(walked.len(), usize::from(known && creates));
        }
    }

    #[test]
    fn moves_into_created_directories_walk_only_outermost_creations_and_keep_reported_moves() {
        let moved = FileIdentity {
            device: 1,
            inode: 2,
            created: std::time::UNIX_EPOCH,
        };
        let from = PathBuf::from("/src/Module.luau");
        let destination = PathBuf::from("/src/New/Deep/Module.luau");
        let before = HashMap::from([(from.clone(), moved)]);
        let events = vec![
            VfsEvent::Create("/src/New".into()),
            VfsEvent::Create("/src/New/Deep".into()),
            VfsEvent::Remove(from.clone()),
            VfsEvent::Rename {
                from: "/src/Other.luau".into(),
                to: destination.clone(),
                identity: None,
            },
        ];
        let mut walked = Vec::new();
        let linked =
            link_moves_into_created_directories(events, &before, &mut HashMap::new(), |root| {
                walked.push(root.to_path_buf());
                vec![(destination.clone(), moved)]
            });
        assert_eq!(walked, vec![PathBuf::from("/src/New")]);
        // A destination the watcher already attributed to another move stays a removal.
        assert!(linked
            .iter()
            .any(|event| matches!(event, VfsEvent::Remove(path) if path == &from)));
    }

    #[cfg(unix)]
    #[test]
    fn identities_under_lists_nested_entries_without_following_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("New");
        std::fs::create_dir_all(root.join("Deep")).unwrap();
        std::fs::write(root.join("Deep/Module.luau"), "return 1\n").unwrap();
        let outside = directory.path().join("Outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("Hidden.luau"), "return 2\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("Link")).unwrap();
        let mut paths: Vec<_> = identities_under(&root)
            .into_iter()
            .map(|(path, _)| path.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("Deep"), PathBuf::from("Deep/Module.luau")]
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stationary_hard_link_alias_delete_create_is_not_inferred_as_a_rename() {
        let root = tempfile::tempdir().unwrap();
        let stationary = root.path().join("stationary");
        let from = root.path().join("old");
        let to = root.path().join("new");
        std::fs::write(&stationary, "unchanged").unwrap();
        assert!(observed_identity(&stationary).is_some());
        std::fs::hard_link(&stationary, &from).unwrap();
        let backend = StdBackend::new().unwrap();
        for path in [&stationary, &from] {
            backend.metadata(path).unwrap();
            assert_eq!(backend.file_identity(path).unwrap(), None);
        }
        let before = backend.identities.lock().unwrap().clone();
        std::fs::remove_file(&from).unwrap();
        std::fs::hard_link(&stationary, &to).unwrap();
        let after = observed_identity(&to)
            .map(|identity| HashMap::from([(to.clone(), identity)]))
            .unwrap_or_default();
        assert_eq!(backend.file_identity(&to).unwrap(), None);
        let events = link_identity_renames(
            vec![VfsEvent::Remove(from.clone()), VfsEvent::Create(to.clone())],
            &before,
            &after,
        );
        assert!(
            matches!(&events[..], [VfsEvent::Remove(old), VfsEvent::Create(new)]
            if old == &from && new == &to)
        );
        std::fs::remove_file(&to).unwrap();
        assert!(backend.file_identity(&stationary).unwrap().is_some());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn directory_link_counts_do_not_disqualify_native_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("child")).unwrap();
        assert!(observed_identity(root.path()).is_some());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn settled_rename_identity_differs_after_destination_replacement() {
        let root = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(root.path()).unwrap();
        let from = root.join("old");
        let to = root.join("new");
        let replacement = root.join("replacement");
        std::fs::write(&from, "same contents").unwrap();
        std::fs::write(&replacement, "same contents").unwrap();
        let mut backend = StdBackend::new().unwrap();
        backend.metadata(&from).unwrap();
        let receiver = backend.event_receiver();
        backend.watch(&root).unwrap();
        thread::sleep(Duration::from_millis(500));
        std::fs::rename(&from, &to).unwrap();
        let events = collect_until(&receiver, |events| {
            events.iter().any(|event| {
                matches!(event,
                VfsEvent::Rename { from: old, to: new, .. } if old == &from && new == &to)
            })
        });
        let identity = events
            .iter()
            .find_map(|event| match event {
                VfsEvent::Rename {
                    from: old,
                    to: new,
                    identity,
                } if old == &from && new == &to => *identity,
                _ => None,
            })
            .expect("A settled native rename must carry its destination identity.");
        let vfs = crate::Vfs::new(backend);
        assert_eq!(vfs.file_identity(&to).unwrap(), Some(identity));
        std::fs::remove_file(&to).unwrap();
        std::fs::rename(&replacement, &to).unwrap();
        let current = vfs.file_identity(&to).unwrap().unwrap();
        assert_ne!(identity, current);
        assert_eq!(vfs.file_identity(&from).unwrap(), None);
    }

    #[test]
    fn observed_replacement_invalidates_an_earlier_native_rename() {
        let old = FileIdentity {
            device: 1,
            inode: 2,
            created: std::time::UNIX_EPOCH,
        };
        let new = FileIdentity { inode: 3, ..old };
        let before = HashMap::from([(PathBuf::from("/a"), old)]);
        let after = HashMap::from([(PathBuf::from("/b"), new)]);
        let events = link_identity_renames(
            vec![VfsEvent::Rename {
                from: "/a".into(),
                to: "/b".into(),
                identity: None,
            }],
            &before,
            &after,
        );
        assert!(matches!(
            &events[..],
            [VfsEvent::Remove(_), VfsEvent::Create(_)]
        ));
    }

    #[test]
    fn queued_directory_renames_carry_descendant_identity_without_a_consumer_read() {
        let root = FileIdentity {
            device: 1,
            inode: 2,
            created: std::time::UNIX_EPOCH,
        };
        let child = FileIdentity { inode: 3, ..root };
        let mut known = HashMap::from([
            (PathBuf::from("/a"), root),
            (PathBuf::from("/a/Child"), child),
        ]);
        let events = vec![VfsEvent::Rename {
            from: "/a".into(),
            to: "/b".into(),
            identity: None,
        }];
        update_known_identities(
            &mut known,
            &events,
            HashMap::from([(PathBuf::from("/b"), root)]),
        );
        assert!(!known.contains_key(Path::new("/a/Child")));
        assert_eq!(known.get(Path::new("/b/Child")), Some(&child));
        let events = link_identity_renames(
            vec![
                VfsEvent::Remove("/b/Child".into()),
                VfsEvent::Create("/b/Renamed".into()),
            ],
            &known,
            &HashMap::from([(PathBuf::from("/b/Renamed"), child)]),
        );
        assert!(
            matches!(&events[..], [VfsEvent::Rename { from, to, .. }] if from == Path::new("/b/Child") && to == Path::new("/b/Renamed"))
        );
        update_known_identities(&mut known, &[VfsEvent::Remove("/b".into())], HashMap::new());
        assert!(known.is_empty());
    }

    #[test]
    fn settled_bursts_are_complete_separate_channel_messages() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let from = PathBuf::from("old");
        let to = PathBuf::from("new");
        let other = PathBuf::from("other");
        let mut first = WatchBatch::default();
        first.record(from.clone(), notify::Op::RENAME, None);
        first.record(to.clone(), notify::Op::RENAME, None);
        let mut second = WatchBatch::default();
        second.record(other.clone(), notify::Op::WRITE, None);

        assert!(send_batch(&sender, first.settle(|path| path == to)));
        assert!(send_batch(&sender, Vec::new()));
        assert!(send_batch(&sender, second.settle(|_| true)));

        let VfsEvent::Batch(first) = receiver.try_recv().unwrap() else {
            panic!("A settled burst must be delivered as one batch.");
        };
        assert_eq!(first.len(), 2);
        assert!(matches!(&first[0], VfsEvent::Remove(path) if path == &from));
        assert!(matches!(&first[1], VfsEvent::Create(path) if path == &to));
        let VfsEvent::Batch(second) = receiver.try_recv().unwrap() else {
            panic!("A later burst must remain a separate batch.");
        };
        assert_eq!(second.len(), 1);
        assert!(matches!(&second[0], VfsEvent::Write(path) if path == &other));
        assert!(receiver.try_recv().is_err());
    }

    /// Collects watcher events until `done` is satisfied or the time runs out.
    fn collect_until(
        receiver: &Receiver<VfsEvent>,
        done: impl Fn(&[VfsEvent]) -> bool,
    ) -> Vec<VfsEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while !done(&events) && Instant::now() < deadline {
            if let Ok(event) = receiver.recv_timeout(Duration::from_millis(100)) {
                match event {
                    VfsEvent::Batch(batch) => events.extend(batch),
                    event => events.push(event),
                }
            }
        }
        events
    }

    #[test]
    fn unwatching_unregistered_descendants_keeps_recursive_watching_active() {
        let root = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(root.path()).unwrap();
        let nested = root.join("Nested");
        std::fs::create_dir(&nested).unwrap();
        let mut backend = StdBackend::new().unwrap();
        let receiver = backend.event_receiver();
        backend.watch(&root).unwrap();
        backend.watch(&nested).unwrap();
        // Descendants are covered by the registered recursive watch, not separate watches.
        thread::sleep(Duration::from_millis(500));
        backend.unwatch(&nested).unwrap();
        for index in 0..32 {
            backend
                .unwatch(&nested.join(format!("save-{index}.tmp")))
                .unwrap();
        }
        let feedback = nested.join("Feedback.txt");
        std::fs::write(&feedback, "External edit.").unwrap();
        let changed = |events: &[VfsEvent]| {
            events.iter().any(|event| match event {
                VfsEvent::Create(path) | VfsEvent::Write(path) => path == &feedback,
                _ => false,
            })
        };
        let events = collect_until(&receiver, changed);
        assert!(changed(&events), "The parent watch lost the external edit.");
        backend.unwatch(&root).unwrap();
        backend.unwatch(&root).unwrap();
    }

    // Moving a folder soon after creating it is what lost the old path on
    // macOS. Both sides of the move must be reported on every platform.
    #[test]
    fn reports_both_sides_of_moving_a_new_folder() {
        let root = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(root.path()).unwrap();
        std::fs::create_dir(root.join("Inner")).unwrap();

        let mut backend = StdBackend::new().unwrap();
        let receiver = backend.event_receiver();
        backend.watch(&root).unwrap();
        // Let the watcher start before the changes it should see.
        thread::sleep(Duration::from_millis(500));

        let from = root.join("Folder");
        let to = root.join("Inner").join("Folder");
        std::fs::create_dir(&from).unwrap();
        thread::sleep(Duration::from_millis(500));
        std::fs::rename(&from, &to).unwrap();

        let removed = |events: &[VfsEvent]| {
            events
                .iter()
                .any(|event| matches!(event, VfsEvent::Remove(path) | VfsEvent::Rename { from: path, .. } if path == &from))
        };
        let created = |events: &[VfsEvent]| {
            events
                .iter()
                .any(|event| matches!(event, VfsEvent::Create(path) | VfsEvent::Rename { to: path, .. } if path == &to))
        };
        let events = collect_until(&receiver, |events| removed(events) && created(events));
        assert!(
            removed(&events),
            "no removal of the old path in {:?}",
            events
        );
        assert!(
            created(&events),
            "no creation of the new path in {:?}",
            events
        );
    }

    // On a case-insensitive filesystem the old name still resolves, so only an
    // exact name check sees that it is gone.
    #[test]
    fn reports_a_case_only_rename() {
        let root = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(root.path()).unwrap();

        let from = root.join("folder");
        let to = root.join("FOLDER");
        std::fs::create_dir(&from).unwrap();

        let mut backend = StdBackend::new().unwrap();
        let receiver = backend.event_receiver();
        backend.metadata(&from).unwrap();
        backend.watch(&root).unwrap();
        thread::sleep(Duration::from_millis(500));

        std::fs::rename(&from, &to).unwrap();

        let removed = |events: &[VfsEvent]| {
            events
                .iter()
                .any(|event| matches!(event, VfsEvent::Remove(path) | VfsEvent::Rename { from: path, .. } if path == &from))
        };
        let events = collect_until(&receiver, removed);
        assert!(
            removed(&events),
            "no removal of the old name in {:?}",
            events
        );
        assert!(events.iter().any(|event| {
            matches!(event, VfsEvent::Rename { from: old, to: new, .. } if old == &from && new == &to)
        }), "The native watcher did not link the case-only rename. {:?}", events);
    }

    #[test]
    fn lock_subprocess_helper() {
        use std::io::{Read, Write};
        let Some(path) = std::env::var_os("MEMOFS_LOCK_TEST_PATH") else {
            return;
        };
        let backend = StdBackend::new().unwrap();
        println!("Waiting.");
        std::io::stdout().flush().unwrap();
        let _guard = backend.lock_exclusive(Path::new(&path)).unwrap();
        println!("Locked.");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read(&mut [0]);
    }

    #[test]
    fn exclusive_lock_blocks_processes_and_releases_on_exit_or_crash() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/relocations.lock");
        let backend = StdBackend::new().unwrap();
        for crash in [false, true] {
            let guard = backend.lock_exclusive(&path).unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "std_backend::tests::lock_subprocess_helper",
                    "--nocapture",
                ])
                .env("MEMOFS_LOCK_TEST_PATH", &path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let output = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            let reader = thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let line = line.unwrap();
                    if line == "Waiting." || line == "Locked." {
                        let _ = tx.send(line);
                    }
                }
            });
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                "Waiting."
            );
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(150)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(guard);
            assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "Locked.");
            let probe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert!(matches!(
                probe.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            if crash {
                child.kill().unwrap();
            } else {
                child.stdin.take().unwrap().write_all(b"x").unwrap();
            }
            let status = child.wait().unwrap();
            assert_eq!(status.success(), !crash);
            reader.join().unwrap();
            let _released = backend.lock_exclusive(&path).unwrap();
        }
    }

    #[test]
    fn exclusive_locks_support_relative_paths() {
        let backend = StdBackend::new().unwrap();
        let directory = tempfile::tempdir_in(".").unwrap();
        let relative_directory = Path::new(directory.path().file_name().unwrap());
        drop(
            backend
                .lock_exclusive(&relative_directory.join("relocations.lock"))
                .unwrap(),
        );
        let file = tempfile::NamedTempFile::new_in(".").unwrap();
        let relative_file = Path::new(file.path().file_name().unwrap());
        drop(backend.lock_exclusive(relative_file).unwrap());
    }

    #[test]
    fn virtual_backend_locks_do_not_touch_the_host_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("absent/relocations.lock");
        for vfs in [
            crate::Vfs::new(crate::InMemoryFs::new()),
            crate::Vfs::new(crate::NoopBackend::new()),
        ] {
            drop(vfs.lock_exclusive(&path).unwrap());
            drop(vfs.lock().lock_exclusive(&path).unwrap());
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn exclusive_lock_preserves_contents_and_rejects_non_regular_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("relocations.lock");
        let backend = StdBackend::new().unwrap();
        std::fs::write(&path, b"preserve").unwrap();
        drop(backend.lock_exclusive(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(backend.lock_exclusive(&link).is_err());
            let hard_link = temp.path().join("hard-link");
            std::fs::hard_link(&path, &hard_link).unwrap();
            assert!(backend.lock_exclusive(&hard_link).is_err());
            let linked_parent = temp.path().join("linked-parent");
            std::os::unix::fs::symlink(temp.path(), &linked_parent).unwrap();
            assert!(backend
                .lock_exclusive(&linked_parent.join("new.lock"))
                .is_err());
            assert!(!temp.path().join("new.lock").exists());
        }
        assert!(backend.lock_exclusive(temp.path()).is_err());
    }

    #[test]
    fn atomic_write_rejects_changed_or_unexpected_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        let mut backend = StdBackend::new().unwrap();
        fs_err::write(&path, b"disk").unwrap();
        for expected in [None, Some(b"stale".as_slice())] {
            let error = backend
                .write_atomic(&path, b"studio", expected)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert_eq!(fs_err::read(&path).unwrap(), b"disk");
        }
        backend
            .write_atomic(&path, b"studio", Some(b"disk"))
            .unwrap();
        assert_eq!(fs_err::read(&path).unwrap(), b"studio");
        assert_eq!(fs_err::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_creates_a_missing_file_without_clobbering() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("metadata");
        let mut backend = StdBackend::new().unwrap();
        backend.write_atomic(&path, b"new", None).unwrap();
        assert_eq!(fs_err::read(&path).unwrap(), b"new");
        assert!(backend.write_atomic(&path, b"other", None).is_err());
        assert_eq!(fs_err::read(&path).unwrap(), b"new");
    }

    #[test]
    fn failed_watch_registration_can_be_retried() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing");
        let mut backend = StdBackend::new().unwrap();
        assert!(backend.watch(&path).is_err());
        assert!(!backend.watches.contains(&path));
        std::fs::create_dir(&path).unwrap();
        backend.watch(&path).unwrap();
        assert!(backend.watches.contains(&path));
    }
}
