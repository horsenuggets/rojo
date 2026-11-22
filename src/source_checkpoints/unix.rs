//! Descriptor-relative storage. Paths are used only to acquire the project-root anchor;
//! retained handles, never reconstructed paths, resolve everything beneath it.

use std::{
    ffi::{OsStr, OsString},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
};

use rustix::fs::{self as at, AtFlags, Mode, OFlags};

use super::{ensure, fs::Metadata, File, OpenOptions, Path, PathBuf, Write};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) struct Namespace {
    root_parent: File,
    root_name: OsString,
    root: File,
    directories: Vec<(OsString, File)>,
}

#[cfg(test)]
struct OperationHook {
    operation: &'static str,
    action: Box<dyn FnOnce()>,
}

#[cfg(test)]
thread_local! {
    static BEFORE_OPERATION: std::cell::RefCell<Option<OperationHook>> = const { std::cell::RefCell::new(None) };
    static FAIL_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn sync_parent(parent: &File) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_PARENT_SYNC.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other(
            "Injected parent directory synchronization failure",
        ));
    }
    parent.sync_all()
}

#[cfg(test)]
fn before_operation(operation: &str) {
    let hook = BEFORE_OPERATION.with_borrow_mut(|hook| {
        if hook
            .as_ref()
            .is_some_and(|hook| hook.operation == operation)
        {
            hook.take()
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        (hook.action)();
    }
}

fn validate_directory(metadata: &Metadata, private: bool, owner: u32) -> anyhow::Result<()> {
    ensure!(
        metadata.is_dir()
            && metadata.uid() == owner
            && metadata.mode() & if private { 0o077 } else { 0o022 } == 0,
        "Checkpoint directory must be owned by the current user and have safe permissions. Restrict access before retrying"
    );
    Ok(())
}

fn validate_file(metadata: &Metadata, owner: u32) -> anyhow::Result<()> {
    ensure!(
        metadata.is_file()
            && metadata.uid() == owner
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "Checkpoint file is not private, is not owned by the current user, or has additional hard links. Preserve it and restrict access before retrying"
    );
    Ok(())
}

fn same_entry(parent: &File, name: &OsStr, file: &File) -> anyhow::Result<()> {
    let entry = at::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let opened = at::fstat(file)?;
    ensure!(
        entry.st_dev == opened.st_dev && entry.st_ino == opened.st_ino,
        "Checkpoint storage namespace changed. Preserve the cache and reconcile manually before retrying"
    );
    Ok(())
}

impl Namespace {
    pub(super) fn open(root: &Path, key: &str) -> anyhow::Result<Self> {
        let root_parent = OpenOptions::new()
            .read(true)
            .custom_flags(DIRECTORY_FLAGS.bits() as i32)
            .open(root.parent().unwrap_or(root))?;
        let root_name = root.file_name().unwrap_or(OsStr::new("."));
        let root = File::from(at::openat(
            &root_parent,
            root_name,
            DIRECTORY_FLAGS,
            Mode::empty(),
        )?);
        validate_directory(
            &root.metadata()?,
            false,
            rustix::process::geteuid().as_raw(),
        )?;
        let mut namespace = Self {
            root_parent,
            root_name: root_name.into(),
            root,
            directories: Vec::new(),
        };
        for (name, private) in [(".rojo", false), ("source-checkpoints", true), (key, true)] {
            namespace.validate()?;
            let parent = namespace.directory();
            match at::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
                Ok(()) => (),
                Err(rustix::io::Errno::EXIST) => (),
                Err(error) => return Err(error.into()),
            }
            let directory = File::from(at::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())?);
            validate_directory(
                &directory.metadata()?,
                private,
                rustix::process::geteuid().as_raw(),
            )?;
            same_entry(parent, OsStr::new(name), &directory)?;
            // Another opener may have created this entry without synchronizing its parent.
            sync_parent(parent)?;
            namespace.directories.push((name.into(), directory));
        }
        namespace.validate()?;
        Ok(namespace)
    }

    fn directory(&self) -> &File {
        self.directories.last().map_or(&self.root, |(_, file)| file)
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        same_entry(&self.root_parent, &self.root_name, &self.root)?;
        validate_directory(
            &self.root.metadata()?,
            false,
            rustix::process::geteuid().as_raw(),
        )?;
        let mut parent = &self.root;
        for (index, (name, directory)) in self.directories.iter().enumerate() {
            let metadata = directory.metadata()?;
            validate_directory(&metadata, index != 0, rustix::process::geteuid().as_raw())?;
            same_entry(parent, name, directory)?;
            parent = directory;
        }
        Ok(())
    }

    pub(super) fn entries(&self) -> anyhow::Result<Vec<PathBuf>> {
        self.validate()?;
        let mut entries = Vec::new();
        #[cfg(test)]
        before_operation("entries");
        for entry in at::Dir::read_from(self.directory())? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                entries.push(PathBuf::from(OsStr::from_bytes(name)));
            }
        }
        self.validate()?;
        Ok(entries)
    }

    pub(super) fn open_file(&self, name: &OsStr, writable: bool) -> anyhow::Result<File> {
        self.validate()?;
        let flags = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        // NONBLOCK prevents an untrusted special file from blocking before fstat rejects it.
        #[cfg(test)]
        before_operation("open");
        let file = File::from(
            at::openat(
                self.directory(),
                name,
                flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        validate_file(&file.metadata()?, rustix::process::geteuid().as_raw())?;
        self.validate()?;
        Ok(file)
    }

    pub(super) fn lock_file(&self) -> anyhow::Result<File> {
        let name = OsStr::new(".lock");
        match self.create_file(name) {
            Ok(file) => Ok(file),
            Err(error)
                if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::EXIST) =>
            {
                self.open_file(name, true)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn exists(&self, name: &OsStr) -> anyhow::Result<bool> {
        self.validate()?;
        match at::statat(self.directory(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn create_file(&self, name: &OsStr) -> anyhow::Result<File> {
        self.validate()?;
        #[cfg(test)]
        before_operation("create");
        let file = File::from(at::openat(
            self.directory(),
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        validate_file(&file.metadata()?, rustix::process::geteuid().as_raw())?;
        self.validate()?;
        Ok(file)
    }

    pub(super) fn remove_file(&self, name: &OsStr) -> anyhow::Result<()> {
        let file = self.open_file(name, false)?;
        same_entry(self.directory(), name, &file)?;
        #[cfg(test)]
        before_operation("remove");
        at::unlinkat(self.directory(), name, AtFlags::empty())?;
        self.validate()
    }

    pub(super) fn temporary_file(&self, _path: &Path) -> anyhow::Result<Temporary> {
        let name = format!(
            "{}{}{}",
            super::INCOMPLETE_PREFIX,
            uuid::Uuid::new_v4().simple(),
            super::INCOMPLETE_SUFFIX
        );
        Ok(Temporary {
            directory: self.directory().try_clone()?,
            file: Some(self.create_file(OsStr::new(&name))?),
            #[cfg(test)]
            path: _path.join(&name),
            name: name.into(),
        })
    }

    pub(super) fn atomic_write(
        &self,
        path: &Path,
        name: &OsStr,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let temporary = self.temporary_file(path)?;
        let mut file = temporary.as_file();
        file.write_all(bytes)?;
        file.sync_all()?;
        self.validate()?;
        validate_file(&file.metadata()?, rustix::process::geteuid().as_raw())?;
        same_entry(self.directory(), &temporary.name, file)?;
        self.publish(&temporary, name)?;
        // Unlike link/unlink publication, a no-replace rename cannot leave a committed
        // record with two links after a crash, which our privacy checks must reject.
        self.sync()
    }

    fn publish(&self, temporary: &Temporary, name: &OsStr) -> anyhow::Result<()> {
        #[cfg(test)]
        before_operation("publish");
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        {
            at::renameat_with(
                self.directory(),
                &temporary.name,
                self.directory(),
                name,
                at::RenameFlags::NOREPLACE,
            )?;
            Ok(())
        }
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
        {
            let _ = (temporary, name);
            anyhow::bail!("Atomic private checkpoint commits are unsupported on this Unix platform")
        }
    }

    pub(super) fn sync(&self) -> anyhow::Result<()> {
        self.validate()?;
        #[cfg(test)]
        before_operation("sync");
        self.directory().sync_all()?;
        self.validate()
    }
}

pub(super) struct Temporary {
    directory: File,
    file: Option<File>,
    name: OsString,
    #[cfg(test)]
    path: PathBuf,
}

impl Temporary {
    pub(super) fn as_file(&self) -> &File {
        self.file.as_ref().unwrap()
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(super) fn keep(mut self) -> std::io::Result<(File, PathBuf)> {
        Ok((self.file.take().unwrap(), self.path.clone()))
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            // Cleanup follows the retained directory even if its public name is replaced.
            // Never delete a substituted entry, including on a failed commit.
            if file.metadata().is_ok_and(|metadata| {
                validate_file(&metadata, rustix::process::geteuid().as_raw()).is_ok()
            }) && same_entry(&self.directory, &self.name, file).is_ok()
            {
                let _ = at::unlinkat(&self.directory, &self.name, AtFlags::empty());
                let _ = self.directory.sync_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };

    use super::*;
    use crate::source_checkpoints::{
        tests::{project, record},
        Store,
    };

    #[test]
    fn reopening_existing_ancestors_requires_parent_sync_and_preserves_records_on_failure() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let id = store.create(record(&store)).unwrap();
        let path = store.record_path(&id).unwrap();
        let before = fs::read(&path).unwrap();
        drop(store);
        FAIL_PARENT_SYNC.with(|fail| fail.set(true));
        let reopened = Store::open(&project);
        FAIL_PARENT_SYNC.with(|fail| fail.set(false));
        assert!(
            reopened.is_err(),
            "Existing ancestors must be synchronized too"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(reopened);
        let reopened = Store::open(&project).unwrap();
        assert_eq!(reopened.read(&id).unwrap().source, "editor source\n");
    }

    #[test]
    fn directory_and_file_ownership_checks_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = directory.path().metadata().unwrap();
        let owner = metadata.uid();
        let foreign = owner.wrapping_add(1);
        for private in [false, true] {
            assert!(validate_directory(&metadata, private, owner).is_ok());
            assert!(validate_directory(&metadata, private, foreign).is_err());
        }
        let path = directory.path().join("private");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(validate_file(&file.metadata().unwrap(), owner).is_ok());
        assert!(validate_file(&file.metadata().unwrap(), foreign).is_err());
        fs::hard_link(&path, directory.path().join("hardlink")).unwrap();
        assert!(validate_file(&file.metadata().unwrap(), owner).is_err());
    }

    #[test]
    fn replacement_between_validation_and_syscall_never_uses_substituted_namespace() {
        for operation in ["entries", "open", "create", "remove", "publish", "sync"] {
            let (_directory, project) = project();
            let store = Store::open(&project).unwrap();
            let id = store.create(record(&store)).unwrap();
            let original = store.record_path(&id).unwrap();
            let before = fs::read(&original).unwrap();
            let outside = tempfile::tempdir().unwrap();
            let substituted_record = outside.path().join(original.file_name().unwrap());
            fs::write(&substituted_record, &before).unwrap();
            fs::set_permissions(&substituted_record, fs::Permissions::from_mode(0o600)).unwrap();
            let path = store.directory.clone();
            let moved = path.with_file_name("detached");
            let destination = outside.path().to_path_buf();
            BEFORE_OPERATION.with_borrow_mut(|hook| {
                *hook = Some(OperationHook {
                    operation,
                    action: Box::new(move || {
                        fs::rename(&path, &moved).unwrap();
                        symlink(&destination, &path).unwrap();
                    }),
                })
            });
            let result = match operation {
                "entries" => store.list().map(|_| ()),
                "open" => store.read(&id).map(|_| ()),
                "create" => store.create(record(&store)).map(|_| ()),
                "remove" => store.discard(&id),
                "publish" => store.atomic_write(&store.directory.join("new.json"), b"secret"),
                "sync" => store.sync_directory(),
                _ => unreachable!(),
            };
            assert!(
                result.is_err(),
                "Acknowledged replaced namespace during {operation}"
            );
            BEFORE_OPERATION
                .with_borrow(|hook| assert!(hook.is_none(), "Missing hook for {operation}"));
            assert_eq!(fs::read(&substituted_record).unwrap(), before);
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
        }
    }
}
