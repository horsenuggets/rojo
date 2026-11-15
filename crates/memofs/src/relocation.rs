use std::{collections::BTreeMap, io, path::Path};

use crate::{VfsBackend, VfsSnapshot};

#[cfg(target_os = "linux")]
const AT_FDCWD: i32 = -100;
#[cfg(target_os = "linux")]
const RENAME_NOREPLACE: u32 = 1;
#[cfg(target_os = "macos")]
const RENAME_EXCL: u32 = 4;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 8;

pub(crate) fn snapshot_tree<B: VfsBackend + ?Sized>(
    backend: &B,
    path: &Path,
) -> io::Result<VfsSnapshot> {
    if backend.metadata(path)?.is_file() {
        return Ok(VfsSnapshot::file(backend.read(path)?));
    }
    let mut children = BTreeMap::new();
    for child in backend.read_dir(path)? {
        let child = child?;
        let name = child
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Tree contains a non-UTF-8 name")
            })?;
        children.insert(name.to_owned(), backend.snapshot_tree(child.path())?);
    }
    Ok(VfsSnapshot::Dir { children })
}

pub(crate) fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "Filesystem tree changed before relocation",
    )
}

// Never emulate exclusion with exists followed by std::fs::rename. That would
// silently replace an entry created by an editor between those operations.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn renameat2(
            oldfd: i32,
            old: *const std::ffi::c_char,
            newfd: i32,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn renamex_np(
            old: *const std::ffi::c_char,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    // SAFETY: Both C strings are NUL terminated and live throughout the call.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    // RENAME_EXCL, not RENAME_SWAP or the best-effort RENAME_SECLUDE.
    #[cfg(target_os = "macos")]
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut result: Vec<_> = path.as_os_str().encode_wide().collect();
        if result.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path contains NUL",
            ));
        }
        result.push(0);
        Ok(result)
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(old: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source = wide(source)?;
    let destination = wide(destination)?;
    // SAFETY: Both buffers are NUL terminated and live throughout the call.
    // WRITE_THROUGH only. No REPLACE_EXISTING and, importantly, no COPY_ALLOWED.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } != 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn rename_no_replace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "No atomic no-replace rename on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryFs, NoopBackend, StdBackend, Vfs};
    use std::sync::{Arc, Barrier};

    fn exercise(mut backend: impl VfsBackend, root: &Path) {
        let source = root.join("source");
        let destination = root.join("destination");
        backend.create_dir(&source).unwrap();
        backend.create_dir(&source.join("nested")).unwrap();
        backend
            .write(&source.join("nested/unmanaged"), b"keep")
            .unwrap();
        let expected = backend.snapshot_tree(&source).unwrap();
        backend.create_dir(&destination).unwrap();
        assert_eq!(
            backend
                .move_no_replace(&source, &destination, &expected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(backend.snapshot_tree(&source).unwrap(), expected);
        backend.remove_dir_all(&destination).unwrap();
        assert!(backend
            .move_no_replace(&source, &destination, &VfsSnapshot::empty_dir())
            .is_err());
        backend
            .move_no_replace(&source, &destination, &expected)
            .unwrap();
        assert!(!backend.exists(&source).unwrap());
        assert_eq!(backend.snapshot_tree(&destination).unwrap(), expected);
        let paths: Vec<_> = backend
            .read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path().to_owned())
            .collect();
        assert_eq!(paths, vec![destination]);
    }

    #[test]
    fn checked_directory_moves_match_on_real_and_memory_backends() {
        let temp = tempfile::tempdir().unwrap();
        exercise(StdBackend::new().unwrap(), temp.path());
        let root = Path::new("/test");
        let mut memory = InMemoryFs::new();
        memory.create_dir_all(root).unwrap();
        exercise(memory, root);
        let mut noop = NoopBackend::new();
        assert!(noop
            .move_no_replace(Path::new("a"), Path::new("b"), &VfsSnapshot::empty_file())
            .is_err());
        assert!(noop.sync_path(Path::new("a")).is_err());
    }

    #[test]
    fn independent_movers_race_without_overwriting_winner() {
        for real in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().to_owned();
            let mut memory = InMemoryFs::new();
            memory.create_dir_all(&root).unwrap();
            for directory in [false, true] {
                for iteration in 0..12 {
                    let make = || {
                        if real {
                            Vfs::new(StdBackend::new().unwrap())
                        } else {
                            Vfs::new(memory.clone())
                        }
                    };
                    let left = make();
                    let right = make();
                    let source_a = root.join(format!("a-{directory}-{iteration}"));
                    let source_b = root.join(format!("b-{directory}-{iteration}"));
                    let destination = root.join(format!("dest-{directory}-{iteration}"));
                    for (fs, path, content) in [(&left, &source_a, "a"), (&right, &source_b, "b")] {
                        if directory {
                            fs.create_dir(path).unwrap();
                            fs.write(path.join("child"), content).unwrap();
                        } else {
                            fs.write(path, content).unwrap();
                        }
                    }
                    let expected_a = left.snapshot_tree(&source_a).unwrap();
                    let expected_b = right.snapshot_tree(&source_b).unwrap();
                    let barrier = Arc::new(Barrier::new(2));
                    let worker_barrier = Arc::clone(&barrier);
                    let worker_destination = destination.clone();
                    let worker_source = source_a.clone();
                    let worker_expected = expected_a.clone();
                    let worker = std::thread::spawn(move || {
                        worker_barrier.wait();
                        left.move_no_replace(&worker_source, &worker_destination, &worker_expected)
                    });
                    barrier.wait();
                    let b = right.move_no_replace(&source_b, &destination, &expected_b);
                    let a = worker.join().unwrap();
                    assert_ne!(a.is_ok(), b.is_ok());
                    let (loser, winner) = if a.is_ok() {
                        (&source_b, &expected_a)
                    } else {
                        (&source_a, &expected_b)
                    };
                    assert!(right.exists(loser).unwrap());
                    assert_eq!(&right.snapshot_tree(&destination).unwrap(), winner);
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn real_backend_rejects_links_and_never_replaces_dangling_destination() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let mut backend = StdBackend::new().unwrap();
        backend.write(&source, b"source").unwrap();
        symlink("missing", &destination).unwrap();
        assert!(backend
            .move_no_replace(
                &source,
                &destination,
                &VfsSnapshot::file(b"source".to_vec())
            )
            .is_err());
        assert!(std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(backend.snapshot_tree(&destination).is_err());
        let nested = temp.path().join("nested");
        backend.create_dir(&nested).unwrap();
        symlink(&source, nested.join("link")).unwrap();
        assert!(backend.snapshot_tree(&nested).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cross_device_moves_fail_without_copying_or_removing_source() {
        use std::os::unix::fs::MetadataExt;
        let source_dir = tempfile::tempdir().unwrap();
        let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if std::fs::metadata(source_dir.path()).unwrap().dev()
            == std::fs::metadata(other.path()).unwrap().dev()
        {
            return;
        }
        let source = source_dir.path().join("source");
        let destination = other.path().join("destination");
        let mut backend = StdBackend::new().unwrap();
        backend.write(&source, b"keep").unwrap();
        assert_eq!(
            backend
                .move_no_replace(&source, &destination, &VfsSnapshot::file(b"keep".to_vec()))
                .unwrap_err()
                .raw_os_error(),
            Some(18)
        );
        assert_eq!(backend.read(&source).unwrap(), b"keep");
        assert!(!destination.exists());
    }
}
