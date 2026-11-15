use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::Duration;
use std::{collections::HashSet, io};

use crossbeam_channel::Receiver;
use notify::{watcher, DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{DirEntry, IoResultExt, Metadata, ReadDir, VfsBackend, VfsEvent};

/// `VfsBackend` that uses `std::fs` and the `notify` crate.
pub struct StdBackend {
    watcher: Mutex<RecommendedWatcher>,
    watcher_receiver: Receiver<VfsEvent>,
    watches: HashSet<PathBuf>,
}

impl StdBackend {
    pub fn new() -> io::Result<StdBackend> {
        let (notify_tx, notify_rx) = mpsc::channel();
        let watcher = watcher(notify_tx, Duration::from_millis(50)).map_err(io::Error::other)?;

        let (tx, rx) = crossbeam_channel::unbounded();

        thread::spawn(move || {
            for event in notify_rx {
                match event {
                    DebouncedEvent::Create(path) => {
                        tx.send(VfsEvent::Create(path))?;
                    }
                    DebouncedEvent::Write(path) => {
                        tx.send(VfsEvent::Write(path))?;
                    }
                    DebouncedEvent::Remove(path) => {
                        tx.send(VfsEvent::Remove(path))?;
                    }
                    DebouncedEvent::Rename(from, to) => {
                        tx.send(VfsEvent::Remove(from))?;
                        tx.send(VfsEvent::Create(to))?;
                    }
                    _ => {}
                }
            }

            Result::<(), crossbeam_channel::SendError<VfsEvent>>::Ok(())
        });

        Ok(Self {
            watcher: Mutex::new(watcher),
            watcher_receiver: rx,
            watches: HashSet::new(),
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

        Ok(Metadata {
            is_file: inner.is_file(),
        })
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
        self.watches.remove(path);
        self.watcher
            .get_mut()
            .unwrap()
            .unwatch(path)
            .map_err(io::Error::other)
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
