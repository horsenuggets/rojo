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

#[cfg(test)]
mod tests {
    use super::*;

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
