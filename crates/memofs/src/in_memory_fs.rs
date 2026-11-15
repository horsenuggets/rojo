use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};

use crate::{DirEntry, Metadata, ReadDir, VfsBackend, VfsEvent, VfsSnapshot};

/// In-memory filesystem that can be used as a VFS backend.
///
/// Internally reference counted to enable giving a copy to
/// [`Vfs`](struct.Vfs.html) and keeping the original to mutate the filesystem's
/// state with.
#[derive(Debug, Clone)]
pub struct InMemoryFs {
    inner: Arc<Mutex<InMemoryFsInner>>,
}

impl InMemoryFs {
    /// Create a new empty `InMemoryFs`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(InMemoryFsInner::new())),
        }
    }

    /// Load a [`VfsSnapshot`](enum.VfsSnapshot.html) into a subtree of the
    /// in-memory filesystem.
    ///
    /// This function will return an error if the operations required to apply
    /// the snapshot result in errors, like trying to create a file inside a
    /// file.
    pub fn load_snapshot<P: Into<PathBuf>>(
        &mut self,
        path: P,
        snapshot: VfsSnapshot,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.load_snapshot(path.into(), snapshot)
    }

    /// Raises a filesystem change event.
    ///
    /// If this `InMemoryFs` is being used as the backend of a
    /// [`Vfs`](struct.Vfs.html), then any listeners be notified of this event.
    pub fn raise_event(&mut self, event: VfsEvent) {
        let inner = self.inner.lock().unwrap();
        inner.event_sender.send(event).unwrap();
    }
}

impl Default for InMemoryFs {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct InMemoryFsInner {
    entries: HashMap<PathBuf, Entry>,
    orphans: BTreeSet<PathBuf>,

    event_receiver: Receiver<VfsEvent>,
    event_sender: Sender<VfsEvent>,
}

impl InMemoryFsInner {
    fn new() -> Self {
        let (event_sender, event_receiver) = crossbeam_channel::unbounded();

        Self {
            entries: HashMap::new(),
            orphans: BTreeSet::new(),
            event_receiver,
            event_sender,
        }
    }

    fn load_snapshot(&mut self, path: PathBuf, snapshot: VfsSnapshot) -> io::Result<()> {
        if let Some(parent_path) = path.parent() {
            if let Some(parent_entry) = self.entries.get_mut(parent_path) {
                if let Entry::Dir { children } = parent_entry {
                    children.insert(path.clone());
                } else {
                    return must_be_dir(parent_path);
                }
            } else {
                self.orphans.insert(path.clone());
            }
        } else {
            self.orphans.insert(path.clone());
        }

        match snapshot {
            VfsSnapshot::File { contents } => {
                self.entries.insert(path, Entry::File { contents });
            }
            VfsSnapshot::Dir { children } => {
                self.entries.insert(
                    path.clone(),
                    Entry::Dir {
                        children: BTreeSet::new(),
                    },
                );

                for (child_name, child) in children {
                    let full_path = path.join(child_name);
                    self.load_snapshot(full_path, child)?;
                }
            }
        }

        Ok(())
    }

    fn snapshot_tree(&self, path: &Path) -> io::Result<VfsSnapshot> {
        match self.entries.get(path) {
            Some(Entry::File { contents }) => Ok(VfsSnapshot::file(contents.clone())),
            Some(Entry::Dir { children }) => {
                let mut snapshots = std::collections::BTreeMap::new();
                for child in children {
                    let name = child
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Tree contains a non-UTF-8 name",
                            )
                        })?;
                    snapshots.insert(name.to_owned(), self.snapshot_tree(child)?);
                }
                Ok(VfsSnapshot::Dir {
                    children: snapshots,
                })
            }
            None => not_found(path),
        }
    }

    fn remove(&mut self, root_path: PathBuf) {
        if let Some(Entry::Dir { children }) = root_path
            .parent()
            .and_then(|parent| self.entries.get_mut(parent))
        {
            children.remove(&root_path);
        }
        self.orphans.remove(&root_path);

        let mut to_remove = VecDeque::new();
        to_remove.push_back(root_path);

        while let Some(path) = to_remove.pop_front() {
            if let Some(Entry::Dir { children }) = self.entries.remove(&path) {
                to_remove.extend(children);
            }
        }
    }
}

#[derive(Debug)]
enum Entry {
    File { contents: Vec<u8> },

    Dir { children: BTreeSet<PathBuf> },
}

impl VfsBackend for InMemoryFs {
    fn is_absolute_path(&self, path: &Path) -> bool {
        path.has_root()
    }

    fn lock_exclusive(&self, _path: &Path) -> io::Result<crate::VfsLockGuard> {
        Ok(crate::VfsLockGuard { _file: None })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let inner = self.inner.lock().unwrap();

        match inner.entries.get(path) {
            Some(Entry::File { contents }) => Ok(contents.clone()),
            Some(Entry::Dir { .. }) => must_be_file(path),
            None => not_found(path),
        }
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        inner.load_snapshot(
            path.to_path_buf(),
            VfsSnapshot::File {
                contents: data.to_owned(),
            },
        )
    }

    fn snapshot_tree(&self, path: &Path) -> io::Result<VfsSnapshot> {
        self.inner.lock().unwrap().snapshot_tree(path)
    }

    fn move_no_replace(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &VfsSnapshot,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if destination.starts_with(source) || source.starts_with(destination) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Move paths overlap",
            ));
        }
        if inner.entries.contains_key(destination) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Destination already exists",
            ));
        }
        let parent = destination.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Destination has no parent")
        })?;
        match inner.entries.get(parent) {
            Some(Entry::Dir { .. }) => {}
            Some(_) => return must_be_dir(parent),
            None => return not_found(parent),
        }
        let snapshot = inner.snapshot_tree(source)?;
        if &snapshot != expected {
            return Err(crate::relocation::changed());
        }
        // The shared mutex covers validation, exclusion and the entire subtree move.
        inner.remove(source.to_owned());
        inner.load_snapshot(destination.to_owned(), snapshot)
    }

    fn copy_permissions(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        let inner = self.inner.lock().unwrap();
        for path in [source, destination] {
            if !inner.entries.contains_key(path) {
                return not_found(path);
            }
        }
        // The virtual filesystem has no host permission state.
        Ok(())
    }

    fn sync_path(&mut self, path: &Path) -> io::Result<()> {
        if self.inner.lock().unwrap().entries.contains_key(path) {
            Ok(())
        } else {
            not_found(path)
        }
    }

    fn write_atomic(
        &mut self,
        path: &Path,
        data: &[u8],
        expected: Option<&[u8]>,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let contents = match inner.entries.get(path) {
            Some(Entry::File { contents }) => Some(contents.as_slice()),
            Some(_) => return must_be_file(path),
            None => None,
        };
        if contents != expected {
            return Err(crate::relocation::changed());
        }
        inner.load_snapshot(path.to_owned(), VfsSnapshot::file(data.to_owned()))
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.entries.contains_key(path))
    }

    fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        let inner = self.inner.lock().unwrap();

        match inner.entries.get(path) {
            Some(Entry::Dir { children }) => {
                let iter = children
                    .clone()
                    .into_iter()
                    .map(|path| Ok(DirEntry { path }));

                Ok(ReadDir {
                    inner: Box::new(iter),
                })
            }
            Some(Entry::File { .. }) => must_be_dir(path),
            None => not_found(path),
        }
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.entries.contains_key(path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Directory already exists",
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            match inner.entries.get(parent) {
                Some(Entry::Dir { .. }) => {}
                Some(_) => return must_be_dir(parent),
                None => return not_found(parent),
            }
        }
        inner.load_snapshot(path.to_path_buf(), VfsSnapshot::empty_dir())
    }

    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            match inner.entries.get(ancestor) {
                Some(Entry::Dir { .. }) => {}
                Some(_) => return must_be_dir(ancestor),
                None => inner.load_snapshot(ancestor.to_owned(), VfsSnapshot::empty_dir())?,
            }
        }
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        match inner.entries.get(path) {
            Some(Entry::File { .. }) => {
                inner.remove(path.to_owned());
                Ok(())
            }
            Some(Entry::Dir { .. }) => must_be_file(path),
            None => not_found(path),
        }
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();

        match inner.entries.get(path) {
            Some(Entry::Dir { .. }) => {
                inner.remove(path.to_owned());
                Ok(())
            }
            Some(Entry::File { .. }) => must_be_dir(path),
            None => not_found(path),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let inner = self.inner.lock().unwrap();

        match inner.entries.get(path) {
            Some(Entry::File { .. }) => Ok(Metadata { is_file: true }),
            Some(Entry::Dir { .. }) => Ok(Metadata { is_file: false }),
            None => not_found(path),
        }
    }

    // TODO: We rely on Rojo to prepend cwd to any relative path before storing paths
    // in MemoFS. The current implementation will error if no prepended absolute path
    // is found. It really only normalizes paths within the provided path's context.
    // Example: "/Users/username/project/../other/file.txt" ->
    // "/Users/username/other/file.txt"
    // Erroneous example: "/Users/../../other/file.txt" -> "/other/file.txt"
    // This is not very robust. We should implement proper path normalization here or otherwise
    // warn if we are missing context and can not fully canonicalize the path correctly.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                std::path::Component::CurDir => {}
                _ => normalized.push(component),
            }
        }

        let inner = self.inner.lock().unwrap();
        match inner.entries.get(&normalized) {
            Some(_) => Ok(normalized),
            None => not_found(&normalized),
        }
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        let inner = self.inner.lock().unwrap();

        inner.event_receiver.clone()
    }

    fn watch(&mut self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn unwatch(&mut self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

fn must_be_file<T>(path: &Path) -> io::Result<T> {
    Err(io::Error::other(format!(
        "path {} was a directory, but must be a file",
        path.display()
    )))
}

fn must_be_dir<T>(path: &Path) -> io::Result<T> {
    Err(io::Error::other(format!(
        "path {} was a file, but must be a directory",
        path.display()
    )))
}

fn not_found<T>(path: &Path) -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("path {} not found", path.display()),
    ))
}
