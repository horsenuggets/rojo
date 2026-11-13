use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use memofs::{IoResultExt, Vfs};

use crate::snapshot_middleware::dir_init_files;

/// A simple representation of a subsection of a file system.
#[derive(Default)]
pub struct FsSnapshot {
    /// Paths representing new files mapped to their contents.
    added_files: HashMap<PathBuf, Vec<u8>>,
    /// Paths representing new directories.
    added_dirs: HashSet<PathBuf>,
    /// Paths representing removed files.
    removed_files: HashSet<PathBuf>,
    /// Paths representing removed directories.
    removed_dirs: HashSet<PathBuf>,
}

impl FsSnapshot {
    /// Creates a new `FsSnapshot`.
    pub fn new() -> Self {
        Self {
            added_files: HashMap::new(),
            added_dirs: HashSet::new(),
            removed_files: HashSet::new(),
            removed_dirs: HashSet::new(),
        }
    }

    /// Adds the given path to the `FsSnapshot` as a file with the given
    /// contents, then returns it.
    pub fn with_added_file<P: AsRef<Path>>(mut self, path: P, data: Vec<u8>) -> Self {
        self.added_files.insert(path.as_ref().to_path_buf(), data);
        self
    }

    /// Adds the given path to the `FsSnapshot` as a file with the given
    /// then returns it.
    pub fn with_added_dir<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.added_dirs.insert(path.as_ref().to_path_buf());
        self
    }

    /// Merges two `FsSnapshot`s together.
    #[inline]
    pub fn merge(&mut self, other: Self) {
        self.added_files.extend(other.added_files);
        self.added_dirs.extend(other.added_dirs);
        self.removed_files.extend(other.removed_files);
        self.removed_dirs.extend(other.removed_dirs);
    }

    /// Merges two `FsSnapshot`s together, with a filter applied to the paths.
    #[inline]
    pub fn merge_with_filter<F>(&mut self, other: Self, mut predicate: F)
    where
        F: FnMut(&Path) -> bool,
    {
        self.added_files
            .extend(other.added_files.into_iter().filter(|(k, _)| predicate(k)));
        self.added_dirs
            .extend(other.added_dirs.into_iter().filter(|p| predicate(p)));
        self.removed_files
            .extend(other.removed_files.into_iter().filter(|p| predicate(p)));
        self.removed_dirs
            .extend(other.removed_dirs.into_iter().filter(|p| predicate(p)));
    }

    /// Adds the provided path as a file with the given contents.
    pub fn add_file<P: AsRef<Path>>(&mut self, path: P, data: Vec<u8>) {
        self.added_files.insert(path.as_ref().to_path_buf(), data);
    }

    /// Adds the provided path as a directory.
    pub fn add_dir<P: AsRef<Path>>(&mut self, path: P) {
        self.added_dirs.insert(path.as_ref().to_path_buf());
    }

    /// Removes the provided path, as a file.
    pub fn remove_file<P: AsRef<Path>>(&mut self, path: P) {
        self.removed_files.insert(path.as_ref().to_path_buf());
    }

    /// Removes the provided path, as a directory.
    pub fn remove_dir<P: AsRef<Path>>(&mut self, path: P) {
        self.removed_dirs.insert(path.as_ref().to_path_buf());
    }

    /// Writes the `FsSnapshot` to the provided VFS, using the provided `base`
    /// as a root for the other paths in the `FsSnapshot`.
    ///
    /// This includes removals, but makes no effort to minimize work done.
    pub fn write_to_vfs<P: AsRef<Path>>(&self, base: P, vfs: &Vfs) -> io::Result<()> {
        let mut lock = vfs.lock();

        let base_path = base.as_ref();
        for dir_path in &self.added_dirs {
            match lock.create_dir_all(base_path.join(dir_path)) {
                Ok(_) => (),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => (),
                Err(err) => return Err(err),
            };
        }
        for (path, contents) in &self.added_files {
            lock.write(base_path.join(path), contents)?;
        }
        for dir_path in &self.removed_dirs {
            lock.remove_dir_all(base_path.join(dir_path))?;
        }
        for path in &self.removed_files {
            lock.remove_file(base_path.join(path))?;
        }
        drop(lock);

        log::debug!(
            "Wrote {} directories and {} files to the file system",
            self.added_dirs.len(),
            self.added_files.len()
        );
        log::debug!(
            "Removed {} directories and {} files from the file system",
            self.removed_dirs.len(),
            self.removed_files.len()
        );
        Ok(())
    }

    /// Captures raw source bytes before snapshotting or serialization can race an editor.
    pub fn capture_file_contents(
        paths: &[PathBuf],
        vfs: &Vfs,
    ) -> io::Result<HashMap<PathBuf, Option<Vec<u8>>>> {
        let mut lock = vfs.lock();
        let mut expected = HashMap::new();
        let mut pending: Vec<PathBuf> = paths.to_vec();
        while let Some(path) = pending.pop() {
            if lock
                .metadata(&path)
                .with_not_found()?
                .is_some_and(|metadata| metadata.is_dir())
            {
                // Directories don't record their absent init files, since that
                // costs memory for every directory. Capture them here so a write
                // still fails if one appears and changes what the directory is.
                for (_, name) in dir_init_files() {
                    let init_path = path.join(name);
                    if !expected.contains_key(&init_path) {
                        pending.push(init_path);
                    }
                }
                continue;
            }
            let contents = lock
                .read(&path)
                .with_not_found()?
                .map(|bytes| (*bytes).clone());
            expected.insert(path, contents);
        }
        Ok(expected)
    }

    /// Persists a scoped update without allowing partial multi-file writes.
    /// Directory creation and removals require a transaction and are rejected here.
    pub fn write_single_file_to_vfs<P: AsRef<Path>>(
        &self,
        base: P,
        vfs: &Vfs,
        expected: &HashMap<PathBuf, Option<Vec<u8>>>,
    ) -> io::Result<()> {
        let mut lock = vfs.lock();
        let base = base.as_ref();
        if !self.removed_files.is_empty() || !self.removed_dirs.is_empty() {
            return Err(io::Error::other(
                "Scoped updates cannot remove filesystem paths",
            ));
        }
        for path in &self.added_dirs {
            if !lock.metadata(base.join(path))?.is_dir() {
                return Err(io::Error::other(
                    "Scoped updates require existing directories",
                ));
            }
        }
        for (path, previous) in expected {
            let current = lock.read(path).with_not_found()?;
            if current.as_ref().map(|bytes| bytes.as_slice()) != previous.as_deref() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Filesystem changed before persistence",
                ));
            }
        }
        let mut changed = Vec::new();
        for (path, contents) in &self.added_files {
            let path = base.join(path);
            let previous = expected.get(&path).ok_or_else(|| {
                io::Error::other("The write target was not included in filesystem validation")
            })?;
            if previous.as_deref() != Some(contents.as_slice()) {
                changed.push((path, contents, previous));
            }
        }
        if changed.len() > 1 {
            return Err(io::Error::other(
                "Multi-file updates require transactional persistence and are not supported yet",
            ));
        }
        if let Some((path, contents, previous)) = changed.pop() {
            lock.write_atomic(&path, contents, previous.as_deref())?;
        }
        Ok(())
    }

    /// Returns whether writing this snapshot would change any file's contents,
    /// compared with the contents captured in `expected`.
    pub fn changes_files<P: AsRef<Path>>(
        &self,
        base: P,
        expected: &HashMap<PathBuf, Option<Vec<u8>>>,
    ) -> bool {
        let base = base.as_ref();
        self.added_files.iter().any(|(path, contents)| {
            expected
                .get(&base.join(path))
                .is_none_or(|previous| previous.as_deref() != Some(contents.as_slice()))
        })
    }

    /// Returns whether this `FsSnapshot` is empty or not.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.added_files.is_empty()
            && self.added_dirs.is_empty()
            && self.removed_files.is_empty()
            && self.removed_dirs.is_empty()
    }

    /// Changes an added file's destination without changing its contents.
    pub fn retarget_added_file(&mut self, from: &Path, to: &Path) {
        if let Some(contents) = self.added_files.remove(from) {
            self.added_files.insert(to.to_path_buf(), contents);
        }
    }

    /// Retains only added files needed by the scoped update.
    pub fn retain_added_files<F>(&mut self, mut predicate: F)
    where
        F: FnMut(&Path) -> bool,
    {
        self.added_files.retain(|path, _| predicate(path));
    }

    /// Returns a list of paths that would be added by this `FsSnapshot`.
    #[inline]
    pub fn added_paths(&self) -> Vec<&Path> {
        let mut list = Vec::with_capacity(self.added_files.len() + self.added_dirs.len());
        list.extend(self.added_files());
        list.extend(self.added_dirs());

        list
    }

    /// Returns a list of paths that would be removed by this `FsSnapshot`.
    #[inline]
    pub fn removed_paths(&self) -> Vec<&Path> {
        let mut list = Vec::with_capacity(self.removed_files.len() + self.removed_dirs.len());
        list.extend(self.removed_files());
        list.extend(self.removed_dirs());

        list
    }

    /// Returns a list of file paths that would be added by this `FsSnapshot`
    #[inline]
    pub fn added_files(&self) -> Vec<&Path> {
        let mut added_files: Vec<_> = self.added_files.keys().map(PathBuf::as_path).collect();
        added_files.sort_unstable();
        added_files
    }

    /// Returns a list of directory paths that would be added by this `FsSnapshot`
    #[inline]
    pub fn added_dirs(&self) -> Vec<&Path> {
        let mut added_dirs: Vec<_> = self.added_dirs.iter().map(PathBuf::as_path).collect();
        added_dirs.sort_unstable();
        added_dirs
    }

    /// Returns a list of file paths that would be removed by this `FsSnapshot`
    #[inline]
    pub fn removed_files(&self) -> Vec<&Path> {
        let mut removed_files: Vec<_> = self.removed_files.iter().map(PathBuf::as_path).collect();
        removed_files.sort_unstable();
        removed_files
    }

    /// Returns a list of directory paths that would be removed by this `FsSnapshot`
    #[inline]
    pub fn removed_dirs(&self) -> Vec<&Path> {
        let mut removed_dirs: Vec<_> = self.removed_dirs.iter().map(PathBuf::as_path).collect();
        removed_dirs.sort_unstable();
        removed_dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn multi_file_scoped_updates_fail_before_changing_any_file() {
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        backend
            .load_snapshot("/project/a", VfsSnapshot::file("old a"))
            .unwrap();
        backend
            .load_snapshot("/project/b", VfsSnapshot::file("old b"))
            .unwrap();
        let vfs = Vfs::new(backend);
        let expected = FsSnapshot::capture_file_contents(
            &[PathBuf::from("/project/a"), PathBuf::from("/project/b")],
            &vfs,
        )
        .unwrap();
        let staged = FsSnapshot::new()
            .with_added_file("a", b"new a".to_vec())
            .with_added_file("b", b"new b".to_vec());
        assert!(staged
            .write_single_file_to_vfs("/project", &vfs, &expected)
            .is_err());
        assert_eq!(&**vfs.read("/project/a").unwrap(), b"old a");
        assert_eq!(&**vfs.read("/project/b").unwrap(), b"old b");
    }

    #[test]
    fn raw_disk_edits_after_capture_are_never_overwritten() {
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        backend
            .load_snapshot("/project/a", VfsSnapshot::file("old"))
            .unwrap();
        let vfs = Vfs::new(backend);
        let expected =
            FsSnapshot::capture_file_contents(&[PathBuf::from("/project/a")], &vfs).unwrap();
        let staged = FsSnapshot::new().with_added_file("a", b"studio".to_vec());
        vfs.write("/project/a", b"disk edit").unwrap();
        let error = staged
            .write_single_file_to_vfs("/project", &vfs, &expected)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(&**vfs.read("/project/a").unwrap(), b"disk edit");
    }

    #[test]
    fn new_init_files_in_a_captured_directory_block_writes() {
        for (_, init_name) in dir_init_files() {
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot("/project", VfsSnapshot::empty_dir())
                .unwrap();
            backend
                .load_snapshot("/project/Folder", VfsSnapshot::empty_dir())
                .unwrap();
            backend
                .load_snapshot("/project/Folder/init.meta.json", VfsSnapshot::file("{}"))
                .unwrap();
            let vfs = Vfs::new(backend);
            let expected = FsSnapshot::capture_file_contents(
                &[
                    PathBuf::from("/project/Folder"),
                    PathBuf::from("/project/Folder/init.meta.json"),
                ],
                &vfs,
            )
            .unwrap();
            let staged = FsSnapshot::new().with_added_file("init.meta.json", b"{ }".to_vec());

            vfs.write(format!("/project/Folder/{init_name}"), b"")
                .unwrap();

            let error = staged
                .write_single_file_to_vfs("/project/Folder", &vfs, &expected)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{init_name}");
            assert_eq!(
                &**vfs.read("/project/Folder/init.meta.json").unwrap(),
                b"{}"
            );
        }
    }

    /// A write is only a change when a staged file differs from what was
    /// captured, including files that don't exist yet.
    #[test]
    fn changes_files_compares_staged_files_with_captured_contents() {
        let base = PathBuf::from("/project");
        let expected = HashMap::from([
            (PathBuf::from("/project/a"), Some(b"old".to_vec())),
            (PathBuf::from("/project/missing"), None),
        ]);
        let cases = [
            (FsSnapshot::new(), false),
            (
                FsSnapshot::new().with_added_file("a", b"old".to_vec()),
                false,
            ),
            (
                FsSnapshot::new().with_added_file("a", b"new".to_vec()),
                true,
            ),
            (
                FsSnapshot::new().with_added_file("missing", Vec::new()),
                true,
            ),
            (
                FsSnapshot::new().with_added_file("uncaptured", b"x".to_vec()),
                true,
            ),
        ];
        for (index, (snapshot, changes)) in cases.into_iter().enumerate() {
            assert_eq!(
                snapshot.changes_files(&base, &expected),
                changes,
                "case {index}"
            );
        }
    }

    #[test]
    fn unchanged_staged_files_do_not_prevent_a_single_file_update() {
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        backend
            .load_snapshot("/project/a", VfsSnapshot::file("old a"))
            .unwrap();
        backend
            .load_snapshot("/project/b", VfsSnapshot::file("old b"))
            .unwrap();
        let vfs = Vfs::new(backend);
        let expected = FsSnapshot::capture_file_contents(
            &[PathBuf::from("/project/a"), PathBuf::from("/project/b")],
            &vfs,
        )
        .unwrap();
        let staged = FsSnapshot::new()
            .with_added_file("a", b"new a".to_vec())
            .with_added_file("b", b"old b".to_vec());
        staged
            .write_single_file_to_vfs("/project", &vfs, &expected)
            .unwrap();
        assert_eq!(&**vfs.read("/project/a").unwrap(), b"new a");
        assert_eq!(&**vfs.read("/project/b").unwrap(), b"old b");
    }
}
