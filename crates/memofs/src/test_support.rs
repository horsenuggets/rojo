//! Opt-in backend-boundary instrumentation for tests and measurements only.
//!
//! Counts dispatches into the wrapped backend, not its internal calls or OS syscalls.
//! Paths retain their exact OS-string spelling, including redundant separators. Two-path
//! operations use ordered source/destination keys; pathless operations use an empty key.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{FileIdentity, Metadata, ReadDir, VfsBackend, VfsEvent, VfsLockGuard, VfsSnapshot};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BackendCall {
    pub operation: &'static str,
    pub paths: Vec<OsString>,
}

pub type BackendCounts = BTreeMap<BackendCall, u64>;

#[derive(Default)]
struct Recording {
    enabled: bool,
    counts: BackendCounts,
}

/// A thread-safe handle. Recording is disabled until `start`, excluding fixture setup.
#[derive(Clone, Default)]
pub struct RecordingHandle(Arc<Mutex<Recording>>);

impl RecordingHandle {
    /// Clear earlier counts and enable recording.
    pub fn start(&self) {
        let mut recording = self.0.lock().unwrap();
        recording.counts.clear();
        recording.enabled = true;
    }

    pub fn snapshot(&self) -> BackendCounts {
        self.0.lock().unwrap().counts.clone()
    }

    /// Clear counts without changing whether recording is enabled.
    pub fn reset(&self) {
        self.0.lock().unwrap().counts.clear();
    }

    /// Disable recording and take the counts, leaving the handle empty.
    pub fn stop(&self) -> BackendCounts {
        let mut recording = self.0.lock().unwrap();
        recording.enabled = false;
        std::mem::take(&mut recording.counts)
    }

    fn record(&self, operation: &'static str, paths: &[&Path]) {
        let mut recording = self.0.lock().unwrap();
        if recording.enabled {
            let call = BackendCall {
                operation,
                paths: paths
                    .iter()
                    .map(|path| path.as_os_str().to_owned())
                    .collect(),
            };
            *recording.counts.entry(call).or_default() += 1;
        }
    }
}

type ObservationHook = Box<dyn Fn(&'static str, &Path) + Send + Sync>;

/// Forwards every method directly, including backend-specific overrides. The recorder
/// never holds its mutex across backend I/O. Without a race hook, directory iterators
/// are forwarded unchanged.
pub struct RecordingBackend<B: VfsBackend> {
    backend: B,
    handle: RecordingHandle,
    after_observation: Option<ObservationHook>,
}

impl<B: VfsBackend> RecordingBackend<B> {
    pub fn new(backend: B) -> (Self, RecordingHandle) {
        let handle = RecordingHandle::default();
        (
            Self {
                backend,
                handle: handle.clone(),
                after_observation: None,
            },
            handle,
        )
    }

    /// Inject an external race after successful canonicalization or a fully read
    /// inventory. Only hooked inventories are eagerly collected; ordinary recording
    /// still forwards the original iterator without changing its behavior.
    pub fn with_after_observation(
        mut self,
        hook: impl Fn(&'static str, &Path) + Send + Sync + 'static,
    ) -> Self {
        self.after_observation = Some(Box::new(hook));
        self
    }
}

impl<B: VfsBackend> crate::sealed::Sealed for RecordingBackend<B> {}

impl<B: VfsBackend> VfsBackend for RecordingBackend<B> {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.handle.record("read", &[path]);
        self.backend.read(path)
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.handle.record("write", &[path]);
        self.backend.write(path, data)
    }

    fn write_atomic(
        &mut self,
        path: &Path,
        data: &[u8],
        expected: Option<&[u8]>,
    ) -> io::Result<()> {
        self.handle.record("write_atomic", &[path]);
        self.backend.write_atomic(path, data, expected)
    }

    fn snapshot_tree(&self, path: &Path) -> io::Result<VfsSnapshot> {
        self.handle.record("snapshot_tree", &[path]);
        self.backend.snapshot_tree(path)
    }

    fn move_no_replace(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &VfsSnapshot,
    ) -> io::Result<()> {
        self.handle
            .record("move_no_replace", &[source, destination]);
        self.backend.move_no_replace(source, destination, expected)
    }

    fn copy_permissions(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        self.handle
            .record("copy_permissions", &[source, destination]);
        self.backend.copy_permissions(source, destination)
    }

    fn sync_path(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("sync_path", &[path]);
        self.backend.sync_path(path)
    }

    fn lock_exclusive(&self, path: &Path) -> io::Result<VfsLockGuard> {
        self.handle.record("lock_exclusive", &[path]);
        self.backend.lock_exclusive(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.handle.record("exists", &[path]);
        self.backend.exists(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.handle.record("read_dir", &[path]);
        let entries = self.backend.read_dir(path)?;
        if let Some(hook) = &self.after_observation {
            let entries = entries.collect::<io::Result<Vec<_>>>()?;
            hook("read_dir", path);
            Ok(ReadDir {
                inner: Box::new(entries.into_iter().map(Ok)),
            })
        } else {
            Ok(entries)
        }
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("create_dir", &[path]);
        self.backend.create_dir(path)
    }

    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("create_dir_all", &[path]);
        self.backend.create_dir_all(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.handle.record("metadata", &[path]);
        self.backend.metadata(path)
    }

    fn file_identity(&self, path: &Path) -> io::Result<Option<FileIdentity>> {
        self.handle.record("file_identity", &[path]);
        self.backend.file_identity(path)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("remove_file", &[path]);
        self.backend.remove_file(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("remove_dir_all", &[path]);
        self.backend.remove_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.handle.record("canonicalize", &[path]);
        let canonical = self.backend.canonicalize(path)?;
        if let Some(hook) = &self.after_observation {
            hook("canonicalize", path);
        }
        Ok(canonical)
    }

    fn is_absolute_path(&self, path: &Path) -> bool {
        self.handle.record("is_absolute_path", &[path]);
        self.backend.is_absolute_path(path)
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.handle.record("event_receiver", &[]);
        self.backend.event_receiver()
    }

    fn watch(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("watch", &[path]);
        self.backend.watch(path)
    }

    fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        self.handle.record("unwatch", &[path]);
        self.backend.unwatch(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryFs, StdBackend};

    fn key(operation: &'static str, paths: &[&Path]) -> BackendCall {
        BackendCall {
            operation,
            paths: paths
                .iter()
                .map(|path| path.as_os_str().to_owned())
                .collect(),
        }
    }

    fn error(error: io::Error) -> (io::ErrorKind, Option<i32>, String) {
        (error.kind(), error.raw_os_error(), error.to_string())
    }

    #[test]
    fn recording_exact_calls_reset_and_concurrent_handle() {
        let (backend, handle) = RecordingBackend::new(InMemoryFs::new());
        backend.exists(Path::new("not-recorded")).unwrap();
        assert!(handle.snapshot().is_empty());
        handle.start();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..10 {
                        backend.exists(Path::new("/a//b")).unwrap();
                    }
                });
            }
        });
        backend.exists(Path::new("/a/b")).unwrap();
        assert_eq!(
            handle.snapshot(),
            BTreeMap::from([
                (key("exists", &[Path::new("/a//b")]), 40),
                (key("exists", &[Path::new("/a/b")]), 1),
            ])
        );
        let frozen = handle.snapshot();
        handle.reset();
        assert!(handle.snapshot().is_empty());
        assert_eq!(frozen.values().sum::<u64>(), 41);
        backend.metadata(Path::new("/missing")).unwrap_err();
        assert_eq!(
            handle.stop(),
            BTreeMap::from([(key("metadata", &[Path::new("/missing")]), 1)])
        );
        backend.exists(Path::new("/after-stop")).unwrap();
        assert!(handle.snapshot().is_empty());
        handle.start();
        backend.event_receiver();
        handle.start();
        assert!(handle.stop().is_empty());
    }

    fn transparent_backend<B: VfsBackend>(mut backend: B, root: &Path, native: bool) {
        backend.create_dir_all(root).unwrap();
        let file = root.join("file");
        let missing = root.join("missing");
        backend.write(&file, b"before").unwrap();
        let bytes = backend.read(&file).unwrap();
        let canonical = backend.canonicalize(&file).unwrap();
        let identity = backend.file_identity(&file).unwrap();
        let snapshot = backend.snapshot_tree(root).unwrap();
        let entries = |dir: ReadDir| {
            dir.map(|entry| entry.map(|entry| entry.path().to_owned()))
                .collect::<io::Result<Vec<_>>>()
        };
        let listed = entries(backend.read_dir(root).unwrap()).unwrap();
        let read_error = error(backend.read(&missing).unwrap_err());
        let metadata_error = error(backend.metadata(&missing).unwrap_err());
        let atomic_error = error(
            backend
                .write_atomic(&file, b"bad", Some(b"wrong"))
                .unwrap_err(),
        );
        let namespace = backend.is_absolute_path(Path::new("/virtual"));
        let (mut backend, handle) = RecordingBackend::new(backend);
        handle.start();
        assert_eq!(backend.read(&file).unwrap(), bytes);
        assert_eq!(backend.canonicalize(&file).unwrap(), canonical);
        assert_eq!(backend.file_identity(&file).unwrap(), identity);
        assert_eq!(backend.snapshot_tree(root).unwrap(), snapshot);
        assert_eq!(entries(backend.read_dir(root).unwrap()).unwrap(), listed);
        assert_eq!(error(backend.read(&missing).unwrap_err()), read_error);
        assert_eq!(
            error(backend.metadata(&missing).unwrap_err()),
            metadata_error
        );
        assert_eq!(
            error(
                backend
                    .write_atomic(&file, b"bad", Some(b"wrong"))
                    .unwrap_err()
            ),
            atomic_error
        );
        assert_eq!(backend.is_absolute_path(Path::new("/virtual")), namespace);
        assert!(backend.metadata(root).unwrap().is_dir());
        assert!(backend.metadata(&file).unwrap().is_file());
        assert!(backend.exists(&file).unwrap());
        assert!(!backend.exists(&missing).unwrap());
        backend
            .write_atomic(&file, b"after", Some(b"before"))
            .unwrap();
        let moved = root.join("moved");
        backend
            .move_no_replace(&file, &moved, &VfsSnapshot::file("after"))
            .unwrap();
        backend.write(&file, b"new").unwrap();
        assert_eq!(
            backend
                .move_no_replace(&file, &moved, &VfsSnapshot::file("new"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        backend.copy_permissions(&moved, &file).unwrap();
        backend.sync_path(&file).unwrap();
        backend.create_dir(&root.join("dir")).unwrap();
        backend.create_dir_all(&root.join("nested/dir")).unwrap();
        let guard = backend.lock_exclusive(&root.join("lock")).unwrap();
        assert_eq!(guard._file.is_some(), native);
        drop(guard);
        backend.event_receiver();
        backend.watch(root).unwrap();
        backend.unwatch(root).unwrap();
        backend.remove_file(&file).unwrap();
        backend.remove_dir_all(&root.join("nested")).unwrap();
        let counts = handle.stop();
        // Check all methods, including exact ordered arguments for two-path calls.
        assert_eq!(
            counts,
            BTreeMap::from([
                (key("canonicalize", &[&file]), 1),
                (key("copy_permissions", &[&moved, &file]), 1),
                (key("create_dir", &[&root.join("dir")]), 1),
                (key("create_dir_all", &[&root.join("nested/dir")]), 1),
                (key("event_receiver", &[]), 1),
                (key("exists", &[&file]), 1),
                (key("exists", &[&missing]), 1),
                (key("file_identity", &[&file]), 1),
                (key("is_absolute_path", &[Path::new("/virtual")]), 1),
                (key("lock_exclusive", &[&root.join("lock")]), 1),
                (key("metadata", &[&file]), 1),
                (key("metadata", &[&missing]), 1),
                (key("metadata", &[root]), 1),
                (key("move_no_replace", &[&file, &moved]), 2),
                (key("read", &[&file]), 1),
                (key("read", &[&missing]), 1),
                (key("read_dir", &[root]), 1),
                (key("remove_dir_all", &[&root.join("nested")]), 1),
                (key("remove_file", &[&file]), 1),
                (key("snapshot_tree", &[root]), 1),
                (key("sync_path", &[&file]), 1),
                (key("unwatch", &[root]), 1),
                (key("watch", &[root]), 1),
                (key("write", &[&file]), 1),
                (key("write_atomic", &[&file]), 2),
            ])
        );
    }

    #[test]
    fn recording_transparent_memory_values_errors_and_overrides() {
        transparent_backend(InMemoryFs::new(), Path::new("/fixture"), false);
    }

    #[test]
    fn recording_transparent_native_values_errors_and_overrides() {
        let temp = tempfile::tempdir().unwrap();
        transparent_backend(StdBackend::new().unwrap(), temp.path(), true);
    }

    #[cfg(unix)]
    #[test]
    fn recording_preserves_native_snapshot_link_rejection_and_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        let copy = temp.path().join("copy");
        let link = temp.path().join("link");
        std::fs::write(&file, "bytes").unwrap();
        std::fs::write(&copy, "copy").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&file, &link).unwrap();
        let baseline = StdBackend::new().unwrap();
        let expected = error(baseline.snapshot_tree(&link).unwrap_err());
        let (mut backend, handle) = RecordingBackend::new(baseline);
        handle.start();
        assert_eq!(error(backend.snapshot_tree(&link).unwrap_err()), expected);
        backend.copy_permissions(&file, &copy).unwrap();
        backend
            .write_atomic(&file, b"after", Some(b"bytes"))
            .unwrap();
        for path in [&file, &copy] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        assert_eq!(handle.stop().values().sum::<u64>(), 3);
    }
}
