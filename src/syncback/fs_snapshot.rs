use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use memofs::{IoResultExt, Vfs};

use crate::snapshot_middleware::dir_init_files;

use super::lfs::AttributesEdit;

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
    /// Shared attributes stay at their real destinations during instance staging.
    shared_edits: HashMap<PathBuf, AttributesEdit>,
    shared_error: Option<String>,
    package_sources: Option<crate::package_sources::PackageSources>,
    pub(super) package_expected: HashMap<PathBuf, Option<Vec<u8>>>,
}

impl FsSnapshot {
    /// Filter all operation kinds together, before any persistence. A protected
    /// removal cancels the batch because its replacement may be elsewhere.
    pub(crate) fn guard_packages(
        &mut self,
        vfs: &Vfs,
        packages: Option<&crate::package_sources::PackageSources>,
    ) {
        let Some(packages) = packages else {
            return;
        };
        self.package_sources = Some(packages.clone());
        let allowed = |path: &Path, subtree: bool| {
            if let Some(reason) = packages.blocked(vfs, path, subtree) {
                log::warn!("Skipping Studio package difference at {}. {reason}. No files or Studio instances were changed", path.display());
                false
            } else {
                true
            }
        };
        if self.removed_dirs.iter().any(|path| !allowed(path, true))
            || self.removed_files.iter().any(|path| !allowed(path, false))
        {
            self.added_files.clear();
            self.added_dirs.clear();
            self.removed_files.clear();
            self.removed_dirs.clear();
            return;
        }
        self.added_files.retain(|path, contents| {
            // Identical installed files are not differences and need no warning.
            if vfs
                .read(path)
                .ok()
                .is_some_and(|bytes| bytes.as_slice() == contents)
            {
                return false;
            }
            allowed(path, false)
        });
        self.added_dirs.retain(|path| {
            !vfs.metadata(path)
                .ok()
                .is_some_and(|metadata| metadata.is_dir())
                && allowed(path, false)
        });
    }

    fn check_packages(&self, vfs: &Vfs) -> io::Result<()> {
        for (path, expected) in &self.package_expected {
            if vfs
                .read(path)
                .with_not_found()?
                .as_ref()
                .map(|bytes| bytes.as_slice())
                != expected.as_deref()
            {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("Package source changed before persistence at {}. Both edits were preserved", path.display())));
            }
        }
        if let Some(previous) = &self.package_sources {
            let current = crate::package_sources::PackageSources::load(vfs, &previous.root)
                .map_err(io::Error::other)?;
            if current.as_ref() != Some(previous) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Wally package ownership changed before persistence",
                ));
            }
        }
        Ok(())
    }

    fn write_package_batch(&self, base: &Path, vfs: &Vfs) -> io::Result<bool> {
        use super::{relocation::execute_replacements, relocation_transaction::Replacement};
        use memofs::{InMemoryFs, VfsSnapshot};
        let Some(packages) = &self.package_sources else {
            return Ok(false);
        };
        let paths: Vec<_> = self
            .added_paths()
            .into_iter()
            .chain(self.removed_paths())
            .map(|path| base.join(path))
            .collect();
        if !paths.iter().any(|path| packages.is_local_source(path)) {
            return Ok(false);
        }
        let mut roots = std::collections::BTreeSet::new();
        for path in paths {
            let mut path = crate::package_sources::normalize(&path);
            // Only replace changed files or newly created/removed directories.
            // Existing parents and authoritative manifests must not be rewritten
            // merely because a descendant changed.
            while let Some(parent) = path.parent() {
                if vfs.metadata(parent).with_not_found()?.is_some() {
                    break;
                }
                path = parent.to_path_buf();
            }
            roots.insert(path);
        }
        let roots: Vec<_> = roots
            .iter()
            .filter(|path| {
                !roots
                    .iter()
                    .any(|ancestor| ancestor != *path && path.starts_with(ancestor))
            })
            .cloned()
            .collect();
        let mut memory = InMemoryFs::new();
        memory.load_snapshot(&packages.root, VfsSnapshot::empty_dir())?;
        let mut entries = Vec::new();
        for path in roots {
            let expected = vfs.lock().snapshot_tree(&path).with_not_found()?;
            if let Some(snapshot) = &expected {
                memory.load_snapshot(&path, snapshot.clone())?;
            }
            entries.push(Replacement {
                path,
                expected,
                desired: None,
            });
        }
        let staged = Vfs::new(memory);
        for entry in &entries {
            if let Some(parent) = entry.path.parent() {
                staged.create_dir_all(parent)?;
            }
        }
        // Stage with no ownership field so this doesn't recurse into a transaction.
        let writes = Self {
            added_files: self.added_files.clone(),
            added_dirs: self.added_dirs.clone(),
            removed_files: self.removed_files.clone(),
            removed_dirs: self.removed_dirs.clone(),
            shared_edits: self.shared_edits.clone(),
            shared_error: self.shared_error.clone(),
            package_sources: None,
            package_expected: HashMap::new(),
        };
        writes.write_to_vfs(base, &staged)?;
        for entry in &mut entries {
            entry.desired = staged.lock().snapshot_tree(&entry.path).with_not_found()?;
        }
        self.check_packages(vfs)?;
        let cache = packages.root.join(".rojo/relocations");
        vfs.create_dir_all(&cache)?;
        vfs.lock().sync_path(cache.parent().unwrap())?;
        vfs.lock().sync_path(&cache)?;
        let journal = cache.join(uuid::Uuid::new_v4().to_string());
        execute_replacements(vfs, &journal, entries)?;
        // Leave terminal journals to the same startup recovery/retirement policy
        // as ordinary relocations; an interrupted write always retains its intent.
        Ok(true)
    }

    /// A suffix migration is the sole scoped write allowed to remove a source.
    /// Its source and sidecar output are staged and recovered as one transaction.
    pub(crate) fn write_script_migration_to_vfs(
        &self,
        project: &crate::Project,
        vfs: &Vfs,
        expected: &HashMap<PathBuf, Option<Vec<u8>>>,
        ownership: &super::ScriptMigrationOwnership,
    ) -> io::Result<()> {
        use super::relocation_transaction::Replacement;
        use memofs::VfsSnapshot;
        self.check_packages(vfs)?;
        if self.added_dirs.iter().any(|path| {
            !vfs.metadata(project.folder_location().join(path))
                .is_ok_and(|metadata| metadata.is_dir())
        }) || !self.removed_dirs.is_empty()
            || !self.shared_edits.is_empty()
            || self.shared_error.is_some()
        {
            return Err(io::Error::other(
                "Script suffix migrations may only replace files",
            ));
        }
        let mut changes = std::collections::BTreeMap::new();
        for (path, contents) in self.file_changes() {
            let path = project.folder_location().join(path);
            let before = expected.get(&path).ok_or_else(|| {
                io::Error::other(format!(
                    "Missing script migration baseline at {}",
                    path.display()
                ))
            })?;
            if before.as_deref() != contents {
                changes.insert(path, (before, contents));
            }
        }
        let entries = changes
            .into_iter()
            .map(|(path, (before, after))| Replacement {
                path,
                expected: before
                    .as_ref()
                    .map(|bytes| VfsSnapshot::file(bytes.clone())),
                desired: after.map(|bytes| VfsSnapshot::file(bytes.to_vec())),
            })
            .collect();
        let cache = super::relocation::cache_directory(project);
        vfs.create_dir_all(&cache)?;
        vfs.sync_path(project.folder_location())?;
        super::relocation::execute_script_replacements(
            vfs,
            &cache.join(uuid::Uuid::new_v4().to_string()),
            project,
            entries,
            expected,
            ownership,
        )
    }

    /// Creates a new `FsSnapshot`.
    pub fn new() -> Self {
        Self {
            added_files: HashMap::new(),
            added_dirs: HashSet::new(),
            removed_files: HashSet::new(),
            removed_dirs: HashSet::new(),
            shared_edits: HashMap::new(),
            shared_error: None,
            package_sources: None,
            package_expected: HashMap::new(),
        }
    }

    pub(crate) fn plan_terrain_lfs(
        &mut self,
        project: &crate::project::Project,
        vfs: &Vfs,
        target: &Path,
    ) -> anyhow::Result<()> {
        let (path, edit) = super::lfs::plan(project, vfs, target)?;
        self.merge_shared(HashMap::from([(path, edit)]))
    }

    fn merge_shared(&mut self, edits: HashMap<PathBuf, AttributesEdit>) -> anyhow::Result<()> {
        // Validate a copy so a failed merge cannot partly add shared rules.
        let mut merged = self.shared_edits.clone();
        for (path, edit) in edits {
            if let Some(previous) = merged.get_mut(&path) {
                previous.merge(edit)?;
            } else {
                merged.insert(path, edit);
            }
        }
        self.shared_edits = merged;
        Ok(())
    }

    fn normal_change_conflicts(&self, base: &Path, path: &Path) -> bool {
        self.added_files
            .keys()
            .chain(self.removed_files.iter())
            .any(|p| base.join(p) == path)
            || self.added_dirs.iter().any(|p| base.join(p) == path)
            || self
                .removed_dirs
                .iter()
                .any(|p| path.starts_with(base.join(p)))
    }

    fn validate_shared(&self, base: &Path, lock: &mut memofs::VfsLock<'_>) -> io::Result<()> {
        if let Some(error) = &self.shared_error {
            return Err(io::Error::other(error.clone()));
        }
        for (path, edit) in &self.shared_edits {
            if self.normal_change_conflicts(base, path) {
                return Err(io::Error::other(
                    "LFS attributes conflict with a normal filesystem change",
                ));
            }
            let current = lock.read(path).with_not_found()?;
            if current.as_ref().map(|bytes| bytes.as_slice()) != edit.expected.as_deref() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "LFS attributes changed before persistence",
                ));
            }
        }
        Ok(())
    }

    fn establish_shared(&self, lock: &mut memofs::VfsLock<'_>) -> io::Result<()> {
        let mut edits: Vec<_> = self.shared_edits.iter().collect();
        edits.sort_by_key(|(path, _)| *path);
        for (path, edit) in edits {
            // Even unchanged rules must be checked against the backend before a model write.
            lock.write_atomic(path, &edit.contents, edit.expected.as_deref())?;
        }
        Ok(())
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
        if let Err(error) = self.merge_shared(other.shared_edits) {
            self.shared_error = Some(error.to_string());
        }
        self.shared_error = self.shared_error.take().or(other.shared_error);
        self.added_files.extend(other.added_files);
        self.added_dirs.extend(other.added_dirs);
        self.removed_files.extend(other.removed_files);
        self.removed_dirs.extend(other.removed_dirs);
    }

    /// Merges filtered paths, rejecting files claimed by different instances.
    /// Project traversal may serialize the same instance more than once.
    #[inline]
    pub fn merge_with_filter<F>(
        &mut self,
        mut other: Self,
        mut predicate: F,
        owner: rbx_dom_weak::types::Ref,
        owners: &mut HashMap<PathBuf, rbx_dom_weak::types::Ref>,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&Path) -> bool,
    {
        other.added_files.retain(|path, _| predicate(path));
        other.added_dirs.retain(|path| predicate(path));
        other.removed_files.retain(|path| predicate(path));
        other.removed_dirs.retain(|path| predicate(path));
        for path in other.added_files.keys() {
            anyhow::ensure!(
                owners.get(path).is_none_or(|previous| *previous == owner),
                "Cannot stage multiple files at the same destination {}",
                path.display()
            );
        }
        anyhow::ensure!(
            self.shared_error.is_none() && other.shared_error.is_none(),
            "Invalid shared attributes plan"
        );
        for path in other.shared_edits.keys() {
            anyhow::ensure!(
                predicate(path),
                "Cannot exclude required LFS attributes {}",
                path.display()
            );
        }
        for path in self.shared_edits.keys().chain(other.shared_edits.keys()) {
            anyhow::ensure!(
                !self.normal_change_conflicts(Path::new(""), path)
                    && !other.normal_change_conflicts(Path::new(""), path),
                "LFS attributes conflict with a normal filesystem change at {}",
                path.display()
            );
        }
        self.merge_shared(other.shared_edits)?;
        owners.extend(other.added_files.keys().cloned().map(|path| (path, owner)));
        self.added_files.extend(other.added_files);
        self.added_dirs.extend(other.added_dirs);
        self.removed_files.extend(other.removed_files);
        self.removed_dirs.extend(other.removed_dirs);
        Ok(())
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

    /// Moves instance-owned added paths from `from` to `to` for staging.
    /// Shared attributes keep their real destinations so Git can apply them before
    /// staged model files are installed. Their parents must already exist.
    /// Fails if the snapshot removes anything or adds an owned path outside `from`.
    pub fn rebase(self, from: &Path, to: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.removed_files.is_empty() && self.removed_dirs.is_empty(),
            "Only new files can be staged"
        );
        let move_path = |path: PathBuf| -> anyhow::Result<PathBuf> {
            let suffix = path
                .strip_prefix(from)
                .map_err(|_| anyhow::anyhow!("{} is outside {}", path.display(), from.display()))?;
            Ok(to.join(suffix))
        };
        let mut rebased = Self::new();
        rebased.shared_edits = self.shared_edits;
        rebased.shared_error = self.shared_error;
        for path in self.added_dirs {
            rebased.added_dirs.insert(move_path(path)?);
        }
        for (path, contents) in self.added_files {
            rebased.added_files.insert(move_path(path)?, contents);
        }
        Ok(rebased)
    }

    /// Writes the `FsSnapshot` to the provided VFS, using the provided `base`
    /// as a root for the other paths in the `FsSnapshot`.
    ///
    /// This includes removals, but makes no effort to minimize work done.
    pub fn write_to_vfs<P: AsRef<Path>>(&self, base: P, vfs: &Vfs) -> io::Result<()> {
        self.check_packages(vfs)?;
        if self.write_package_batch(base.as_ref(), vfs)? {
            return Ok(());
        }
        let mut lock = vfs.lock();

        let base_path = base.as_ref();
        self.validate_shared(base_path, &mut lock)?;
        self.establish_shared(&mut lock)?;
        // Ownership and ignored-file checks happen while planning. Remove old
        // layouts before creating any outputs, including descendants of a reused
        // directory. Skipping an overlapping removal would leave stale old data.
        for dir_path in &self.removed_dirs {
            lock.remove_dir_all(base_path.join(dir_path))?;
        }
        for path in self.removed_files.intersection(&self.added_dirs) {
            lock.remove_file(base_path.join(path))?;
        }
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
        for path in &self.removed_files {
            // An Instance that moves to a new layout is removed and written
            // as new. If it lands on its old path after all, the file that was
            // just written must survive.
            if self.added_files.contains_key(path) || self.added_dirs.contains(path) {
                continue;
            }
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
    /// Shared attributes are checked independently and established before the model.
    pub fn write_single_file_to_vfs<P: AsRef<Path>>(
        &self,
        base: P,
        vfs: &Vfs,
        expected: &HashMap<PathBuf, Option<Vec<u8>>>,
    ) -> io::Result<()> {
        self.check_packages(vfs)?;
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
        self.validate_shared(base, &mut lock)?;
        self.establish_shared(&mut lock)?;
        if let Some((path, contents, previous)) = changed.pop() {
            lock.write_atomic(&path, contents, previous.as_deref())?;
        }
        Ok(())
    }

    /// Persists a rewritten model file along with its adjacent metadata, which
    /// stops setting the properties the model file now holds. Every owned path must
    /// be in `expected` and unchanged. Shared attributes carry their own preconditions
    /// and are established first. The model file precedes metadata, so if
    /// writing stops partway, the metadata still sets the same values as before
    /// and nothing is lost.
    pub fn write_model_file_to_vfs<P: AsRef<Path>>(
        &self,
        base: P,
        vfs: &Vfs,
        expected: &HashMap<PathBuf, Option<Vec<u8>>>,
        model: &Path,
    ) -> io::Result<()> {
        self.check_packages(vfs)?;
        let mut lock = vfs.lock();
        let base = base.as_ref();
        if !self.added_dirs.is_empty() || !self.removed_dirs.is_empty() {
            return Err(io::Error::other(
                "A model file update cannot change directories",
            ));
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
        let mut writes = Vec::new();
        for (path, contents) in &self.added_files {
            let path = base.join(path);
            let previous = expected.get(&path).ok_or_else(|| {
                io::Error::other("The write target was not included in filesystem validation")
            })?;
            if previous.as_deref() != Some(contents.as_slice()) {
                writes.push((path, contents, previous));
            }
        }
        let mut removals = Vec::new();
        for path in &self.removed_files {
            let path = base.join(path);
            if !expected.contains_key(&path) {
                return Err(io::Error::other(
                    "The removed path was not included in filesystem validation",
                ));
            }
            removals.push(path);
        }
        self.validate_shared(base, &mut lock)?;
        self.establish_shared(&mut lock)?;
        writes.sort_by_key(|(path, _, _)| path.as_path() != model);
        for (path, contents, previous) in writes {
            lock.write_atomic(&path, contents, previous.as_deref())?;
        }
        for path in removals {
            lock.remove_file(&path)?;
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
        self.shared_edits
            .values()
            .any(|edit| edit.expected.as_deref() != Some(edit.contents.as_slice()))
            || self.added_files.iter().any(|(path, contents)| {
                expected
                    .get(&base.join(path))
                    .is_none_or(|previous| previous.as_deref() != Some(contents.as_slice()))
            })
    }

    /// Returns whether this `FsSnapshot` is empty or not.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.added_files.is_empty()
            && self.shared_edits.is_empty()
            && self.shared_error.is_none()
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

    pub(crate) fn is_shared_attributes_path(&self, path: &Path) -> bool {
        self.shared_edits.contains_key(path)
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

    /// Returns each file this `FsSnapshot` writes, with its contents, or with
    /// `None` for each file it removes.
    pub fn file_changes(&self) -> Vec<(&Path, Option<&[u8]>)> {
        let written = self
            .added_files
            .iter()
            .map(|(path, contents)| (path.as_path(), Some(contents.as_slice())));
        let removed = self
            .removed_files
            .iter()
            .filter(|path| !self.added_files.contains_key(*path))
            .map(|path| (path.as_path(), None));
        let shared = self
            .shared_edits
            .iter()
            .map(|(path, edit)| (path.as_path(), Some(edit.contents.as_slice())));
        written.chain(shared).chain(removed).collect()
    }

    /// Returns a list of file paths that would be added by this `FsSnapshot`
    #[inline]
    pub fn added_files(&self) -> Vec<&Path> {
        let mut added_files: Vec<_> = self.added_files.keys().map(PathBuf::as_path).collect();
        added_files.extend(self.shared_edits.keys().map(PathBuf::as_path));
        added_files.sort_unstable();
        added_files.dedup();
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

    fn shared_snapshot(target: &str, original: Option<Vec<u8>>) -> FsSnapshot {
        let mut snapshot = FsSnapshot::new().with_added_file(target, b"terrain".to_vec());
        snapshot.shared_edits.insert(
            PathBuf::from("/project/.gitattributes"),
            AttributesEdit::new(
                original,
                format!("\"/{target}\" filter=lfs diff=lfs merge=lfs -text"),
            )
            .unwrap(),
        );
        snapshot
    }

    fn shared_vfs() -> Vfs {
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        backend
            .load_snapshot("/project/Terrain.rbxm", VfsSnapshot::file("old"))
            .unwrap();
        Vfs::new(backend)
    }

    #[test]
    fn recursive_removals_precede_replacement_subtree_writes() {
        for file_to_directory in [false, true] {
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot("/project", VfsSnapshot::empty_dir())
                .unwrap();
            if file_to_directory {
                backend
                    .load_snapshot("/project/Twin", VfsSnapshot::file("old file"))
                    .unwrap();
            } else {
                backend
                    .load_snapshot("/project/Twin", VfsSnapshot::empty_dir())
                    .unwrap();
                backend
                    .load_snapshot("/project/Twin/stale", VfsSnapshot::file("stale"))
                    .unwrap();
            }
            let vfs = Vfs::new(backend);
            let mut snapshot = FsSnapshot::new()
                .with_added_dir("Twin")
                .with_added_dir("Twin/Replacement")
                .with_added_file("Twin/Replacement/data", b"incoming".to_vec());
            if file_to_directory {
                snapshot.remove_file("Twin");
            } else {
                snapshot.remove_dir("Twin");
            }
            snapshot.write_to_vfs("/project", &vfs).unwrap();
            assert_eq!(
                &**vfs.read("/project/Twin/Replacement/data").unwrap(),
                b"incoming"
            );
            assert!(vfs.metadata("/project/Twin/stale").is_err());
        }
    }

    #[test]
    fn shared_plans_are_visible_without_writes_and_survive_rebase() {
        let vfs = shared_vfs();
        let snapshot = shared_snapshot("/project/future/Terrain.rbxm", None);
        assert!(!snapshot.is_empty());
        assert!(snapshot.changes_files("/", &HashMap::new()));
        assert!(snapshot
            .added_paths()
            .contains(&Path::new("/project/.gitattributes")));
        assert!(snapshot.file_changes().iter().any(|(path, contents)| *path
            == Path::new("/project/.gitattributes")
            && contents.is_some()));
        assert!(vfs.read("/project/.gitattributes").is_err());
        let rebased = snapshot
            .rebase(Path::new("/project/future"), Path::new("/project/stage"))
            .unwrap();
        assert!(rebased
            .added_files()
            .contains(&Path::new("/project/.gitattributes")));
        assert!(rebased
            .added_files()
            .contains(&Path::new("/project/stage/Terrain.rbxm")));
        assert!(!rebased
            .added_files()
            .contains(&Path::new("/project/stage/.gitattributes")));
        rebased.write_to_vfs("/", &vfs).unwrap();
        // Only staging may create directories. The future relocation destination stays absent.
        assert!(vfs.read("/project/.gitattributes").is_ok());
        assert!(vfs.metadata("/project/future").is_err());
    }

    #[test]
    fn shared_rules_merge_across_owners_but_inconsistent_preconditions_fail() {
        let mut snapshot = shared_snapshot("a.rbxm", None);
        let mut owners = HashMap::new();
        snapshot
            .merge_with_filter(
                shared_snapshot("b.rbxm", None),
                |_| true,
                rbx_dom_weak::types::Ref::new(),
                &mut owners,
            )
            .unwrap();
        let attrs = &snapshot.shared_edits[Path::new("/project/.gitattributes")];
        let text = std::str::from_utf8(&attrs.contents).unwrap();
        assert!(text.contains("a.rbxm") && text.contains("b.rbxm"));
        let previous = attrs.contents.clone();
        assert!(snapshot
            .merge_with_filter(
                shared_snapshot("c.rbxm", Some(b"edited".to_vec())),
                |_| true,
                rbx_dom_weak::types::Ref::new(),
                &mut owners
            )
            .is_err());
        assert_eq!(
            snapshot.shared_edits[Path::new("/project/.gitattributes")].contents,
            previous
        );
        assert!(!snapshot.added_files.contains_key(Path::new("c.rbxm")));
    }

    #[test]
    fn filtered_merge_rejects_normal_writes_to_shared_destinations() {
        for shared_first in [false, true] {
            let shared = shared_snapshot("/project/Terrain.rbxm", None);
            let normal =
                FsSnapshot::new().with_added_file("/project/.gitattributes", b"normal".to_vec());
            let (mut staged, incoming) = if shared_first {
                (shared, normal)
            } else {
                (normal, shared)
            };
            assert!(staged
                .merge_with_filter(
                    incoming,
                    |_| true,
                    rbx_dom_weak::types::Ref::new(),
                    &mut HashMap::new()
                )
                .is_err());
        }
    }

    #[test]
    fn all_writers_establish_attributes_with_their_own_preconditions() {
        for writer in 0..3 {
            let vfs = shared_vfs();
            let snapshot = shared_snapshot("Terrain.rbxm", None);
            let expected = HashMap::from([(
                PathBuf::from("/project/Terrain.rbxm"),
                Some(b"old".to_vec()),
            )]);
            match writer {
                0 => snapshot.write_to_vfs("/project", &vfs),
                1 => snapshot.write_single_file_to_vfs("/project", &vfs, &expected),
                _ => snapshot.write_model_file_to_vfs(
                    "/project",
                    &vfs,
                    &expected,
                    Path::new("/project/Terrain.rbxm"),
                ),
            }
            .unwrap();
            assert_eq!(&**vfs.read("/project/Terrain.rbxm").unwrap(), b"terrain");
            assert_eq!(
                &**vfs.read("/project/.gitattributes").unwrap(),
                snapshot.shared_edits[Path::new("/project/.gitattributes")].contents
            );
        }
    }

    #[test]
    fn stale_shared_edits_and_normal_write_conflicts_never_write_models() {
        for writer in 0..3 {
            for conflict in [false, true] {
                let vfs = shared_vfs();
                let mut snapshot = shared_snapshot("Terrain.rbxm", None);
                if conflict {
                    snapshot.add_file(".gitattributes", b"normal write".to_vec());
                } else {
                    vfs.write("/project/.gitattributes", b"user edit").unwrap();
                }
                let expected = FsSnapshot::capture_file_contents(
                    &[
                        PathBuf::from("/project/Terrain.rbxm"),
                        PathBuf::from("/project/.gitattributes"),
                    ],
                    &vfs,
                )
                .unwrap();
                let result = match writer {
                    0 => snapshot.write_to_vfs("/project", &vfs),
                    1 => snapshot.write_single_file_to_vfs("/project", &vfs, &expected),
                    _ => snapshot.write_model_file_to_vfs(
                        "/project",
                        &vfs,
                        &expected,
                        Path::new("/project/Terrain.rbxm"),
                    ),
                };
                assert!(result.is_err());
                assert_eq!(&**vfs.read("/project/Terrain.rbxm").unwrap(), b"old");
                if !conflict {
                    assert_eq!(
                        &**vfs.read("/project/.gitattributes").unwrap(),
                        b"user edit"
                    );
                }
            }
        }
    }

    #[test]
    fn raw_attributes_edits_are_rejected_by_every_writer() {
        for writer in 0..3 {
            for original in [None, Some(b"# original\n".to_vec())] {
                let directory = tempfile::tempdir().unwrap();
                let target = directory.path().join("Terrain.rbxm");
                let attributes = directory.path().join(".gitattributes");
                std::fs::write(&target, b"old").unwrap();
                if let Some(bytes) = &original {
                    std::fs::write(&attributes, bytes).unwrap();
                }
                let vfs = Vfs::new_default().unwrap();
                vfs.set_watch_enabled(false);
                let mut staged = FsSnapshot::new().with_added_file(&target, b"terrain".to_vec());
                staged.shared_edits.insert(
                    attributes.clone(),
                    AttributesEdit::new(
                        original,
                        "\"/Terrain.rbxm\" filter=lfs diff=lfs merge=lfs -text".to_owned(),
                    )
                    .unwrap(),
                );
                let expected =
                    FsSnapshot::capture_file_contents(std::slice::from_ref(&target), &vfs).unwrap();
                std::fs::write(&attributes, b"# user edit\n").unwrap();
                let result = match writer {
                    0 => staged.write_to_vfs(directory.path(), &vfs),
                    1 => staged.write_single_file_to_vfs(directory.path(), &vfs, &expected),
                    _ => staged.write_model_file_to_vfs(directory.path(), &vfs, &expected, &target),
                };
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
                assert_eq!(std::fs::read(&target).unwrap(), b"old");
                assert_eq!(std::fs::read(&attributes).unwrap(), b"# user edit\n");
            }
        }
    }

    #[test]
    fn real_staging_establishes_attributes_without_creating_the_install_destination() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let destination = directory.path().join("future");
        let staging = directory.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let attributes = directory.path().join(".gitattributes");
        let mut staged = FsSnapshot::new()
            .with_added_file(destination.join("Terrain.rbxm"), b"terrain".to_vec());
        staged.shared_edits.insert(
            attributes.clone(),
            AttributesEdit::new(
                None,
                "\"/future/Terrain.rbxm\" filter=lfs diff=lfs merge=lfs -text".to_owned(),
            )
            .unwrap(),
        );
        let staged = staged.rebase(&destination, &staging).unwrap();
        staged.write_to_vfs(directory.path(), &vfs).unwrap();
        assert!(attributes.is_file());
        assert!(!destination.exists());
        assert!(staging.join("Terrain.rbxm").is_file());
    }

    #[test]
    fn rebase_moves_added_paths_and_rejects_anything_else() {
        let from = Path::new("/project/src");
        let to = Path::new("/project/.rojo/stage");
        let snapshot = FsSnapshot::new()
            .with_added_dir(from.join("Folder"))
            .with_added_file(from.join("Folder/init.meta.json"), b"{}".to_vec());
        let rebased = snapshot.rebase(from, to).unwrap();
        assert_eq!(rebased.added_dirs(), vec![to.join("Folder").as_path()]);
        assert_eq!(
            rebased.added_files.get(&to.join("Folder/init.meta.json")),
            Some(&b"{}".to_vec())
        );

        let outside = FsSnapshot::new().with_added_file("/elsewhere/file.luau", Vec::new());
        assert!(outside.rebase(from, to).is_err());
        let mut removal = FsSnapshot::new();
        removal.remove_file(from.join("old.luau"));
        assert!(removal.rebase(from, to).is_err());
    }

    #[test]
    fn filtered_merge_rejects_duplicate_destinations_before_mutating() {
        for same_contents in [false, true] {
            for ignored in [false, true] {
                let mut staged = FsSnapshot::new().with_added_file("Code.luau", b"first".to_vec());
                let incoming = FsSnapshot::new()
                    .with_added_file("Other.luau", b"other".to_vec())
                    .with_added_file(
                        "Code.luau",
                        if same_contents { b"first" } else { b"other" }.to_vec(),
                    );
                let original_owner = rbx_dom_weak::types::Ref::new();
                let mut owners = HashMap::from([(PathBuf::from("Code.luau"), original_owner)]);
                let result = staged.merge_with_filter(
                    incoming,
                    |path| !ignored || path != Path::new("Code.luau"),
                    rbx_dom_weak::types::Ref::new(),
                    &mut owners,
                );
                assert_eq!(result.is_ok(), ignored);
                assert_eq!(staged.added_files[Path::new("Code.luau")], b"first");
                assert_eq!(
                    staged.added_files.contains_key(Path::new("Other.luau")),
                    ignored
                );
            }
        }
    }

    #[test]
    fn filtered_merge_allows_the_same_instance_to_reserialize_its_file() {
        let owner = rbx_dom_weak::types::Ref::new();
        let mut owners = HashMap::from([(PathBuf::from("Code.luau"), owner)]);
        let mut staged = FsSnapshot::new().with_added_file("Code.luau", b"first".to_vec());
        staged
            .merge_with_filter(
                FsSnapshot::new().with_added_file("Code.luau", b"updated".to_vec()),
                |_| true,
                owner,
                &mut owners,
            )
            .unwrap();
        assert_eq!(staged.added_files[Path::new("Code.luau")], b"updated");
    }

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
