//! Turns raw filesystem watcher events into [`VfsEvent`]s.
//!
//! The kind a watcher reports for a path can't be trusted. macOS FSEvents keeps
//! a path's earlier flags, so moving a folder a few seconds after creating it
//! reports the old path as both created and renamed, and notify's debouncer
//! then never reports it removed. Instead, events are only used to learn which
//! paths changed. Each batch is settled by checking which of those paths still
//! exist once the burst of events is over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use notify::Op;

use crate::VfsEvent;

/// The paths a burst of raw events touched, in the order they first appeared.
#[derive(Debug, Default)]
pub(crate) struct WatchBatch {
    order: Vec<PathBuf>,
    ops: HashMap<PathBuf, Op>,
}

impl WatchBatch {
    pub(crate) fn record(&mut self, path: PathBuf, op: Op) {
        match self.ops.get_mut(&path) {
            Some(existing) => *existing |= op,
            None => {
                self.order.push(path.clone());
                self.ops.insert(path, op);
            }
        }
    }

    /// Settles the batch. A path that no longer exists was removed, one that
    /// only had its contents written was written, and any other path that
    /// exists was created. Paths that only changed permissions are skipped.
    pub(crate) fn settle(self, mut exists: impl FnMut(&Path) -> bool) -> Vec<VfsEvent> {
        let mut ops = self.ops;
        let mut events = Vec::with_capacity(self.order.len());

        for path in self.order {
            let op = ops.remove(&path).unwrap_or_else(Op::empty);
            if !exists(&path) {
                events.push(VfsEvent::Remove(path));
            } else if op.intersects(Op::CREATE | Op::REMOVE | Op::RENAME) {
                events.push(VfsEvent::Create(path));
            } else if op.intersects(Op::WRITE | Op::CLOSE_WRITE) {
                events.push(VfsEvent::Write(path));
            }
        }

        events
    }
}

/// Returns whether `path` exists with exactly that name. A case-insensitive
/// filesystem also finds a file under a differently cased name, which would
/// hide a rename that only changed case. `entries` caches each directory's
/// listing for the length of one batch.
pub(crate) fn exists_exactly(
    path: &Path,
    entries: &mut HashMap<PathBuf, Option<Vec<std::ffi::OsString>>>,
) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.exists();
    };

    let listing = entries.entry(parent.to_path_buf()).or_insert_with(|| {
        std::fs::read_dir(parent).ok().map(|read| {
            read.filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .collect()
        })
    });

    match listing {
        Some(names) => names.iter().any(|entry| entry == name),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn settle(records: &[(&str, Op)], existing: &[&str]) -> Vec<VfsEvent> {
        let existing: HashSet<PathBuf> = existing.iter().map(PathBuf::from).collect();
        let mut batch = WatchBatch::default();
        for (path, op) in records {
            batch.record(PathBuf::from(path), *op);
        }
        batch.settle(|path| existing.contains(path))
    }

    fn describe(events: &[VfsEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                VfsEvent::Create(path) => format!("Create {}", path.display()),
                VfsEvent::Write(path) => format!("Write {}", path.display()),
                VfsEvent::Remove(path) => format!("Remove {}", path.display()),
            })
            .collect()
    }

    // The bug this module exists for. FSEvents reports the source of a move as
    // created and renamed, which notify's debouncer turned into nothing.
    #[test]
    fn reports_a_moved_path_as_removed_whatever_its_flags_say() {
        let events = settle(
            &[
                ("/src/Folder", Op::CREATE | Op::RENAME),
                ("/src/Inner/Folder", Op::RENAME),
            ],
            &["/src/Inner/Folder"],
        );
        assert_eq!(
            describe(&events),
            ["Remove /src/Folder", "Create /src/Inner/Folder"]
        );
    }

    // Every combination of the ops a raw event can carry, against the path
    // existing or not, settles to exactly the event its rule describes.
    #[test]
    fn settles_every_op_combination_by_existence() {
        let flags = [
            Op::CHMOD,
            Op::CREATE,
            Op::REMOVE,
            Op::RENAME,
            Op::WRITE,
            Op::CLOSE_WRITE,
        ];
        for bits in 1u32..(1 << flags.len()) {
            let mut op = Op::empty();
            for (index, flag) in flags.iter().enumerate() {
                if bits & (1 << index) != 0 {
                    op |= *flag;
                }
            }
            for exists in [false, true] {
                let events = settle(&[("/a", op)], if exists { &["/a"] } else { &[] });
                let expected: Vec<&str> = if !exists {
                    vec!["Remove /a"]
                } else if op.intersects(Op::CREATE | Op::REMOVE | Op::RENAME) {
                    vec!["Create /a"]
                } else if op.intersects(Op::WRITE | Op::CLOSE_WRITE) {
                    vec!["Write /a"]
                } else {
                    vec![]
                };
                assert_eq!(
                    describe(&events),
                    expected,
                    "op {:?}, exists {}",
                    op,
                    exists
                );
            }
        }
    }

    // A path's ops merge, so it is reported once, at the position it first
    // appeared, and the order of the batch follows the order of the burst.
    #[test]
    fn reports_each_path_once_in_first_seen_order() {
        let events = settle(
            &[
                ("/src/A", Op::RENAME),
                ("/src/Temp", Op::RENAME),
                ("/src/B", Op::RENAME),
                ("/src/A", Op::RENAME),
                ("/src/Temp", Op::RENAME),
                ("/src/B", Op::RENAME),
            ],
            &["/src/A", "/src/B"],
        );
        assert_eq!(
            describe(&events),
            ["Create /src/A", "Remove /src/Temp", "Create /src/B"]
        );
    }

    // A file created and deleted inside one burst is gone by the time the
    // batch settles, so the only thing to report is that it doesn't exist.
    #[test]
    fn reports_a_short_lived_path_as_removed() {
        let events = settle(
            &[("/src/Flash", Op::CREATE), ("/src/Flash", Op::REMOVE)],
            &[],
        );
        assert_eq!(describe(&events), ["Remove /src/Flash"]);
    }

    #[test]
    fn checks_existence_with_the_exact_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Lower")).unwrap();
        let mut entries = HashMap::new();
        assert!(exists_exactly(&root.path().join("Lower"), &mut entries));
        // Found case-insensitively on macOS and Windows, but not this name.
        assert!(!exists_exactly(&root.path().join("LOWER"), &mut entries));
        assert!(!exists_exactly(&root.path().join("Missing"), &mut entries));
        assert!(!exists_exactly(
            &root.path().join("Missing").join("Child"),
            &mut entries
        ));
    }
}
