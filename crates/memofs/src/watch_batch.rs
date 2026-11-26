//! Turns raw filesystem watcher events into [`VfsEvent`]s.
//!
//! The kind a watcher reports for a path can't be trusted. macOS FSEvents keeps
//! a path's earlier flags, so moving a folder a few seconds after creating it
//! reports the old path as both created and renamed, and notify's debouncer
//! then never reports it removed. Instead, events are only used to learn which
//! paths changed. Each batch is settled by checking which of those paths still
//! exist once the burst of events is over.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use notify::Op;

use crate::VfsEvent;

/// The paths a burst of raw events touched, in the order they first appeared.
#[derive(Debug, Default)]
pub(crate) struct WatchBatch {
    order: Vec<PathBuf>,
    ops: HashMap<PathBuf, Op>,
    renames: HashMap<u32, Vec<PathBuf>>,
    superseded: HashSet<PathBuf>,
}

impl WatchBatch {
    pub(crate) fn record(&mut self, path: PathBuf, op: Op, cookie: Option<u32>) {
        if !op.contains(Op::RENAME)
            && op.intersects(Op::CREATE | Op::REMOVE)
            && self
                .ops
                .get(&path)
                .is_some_and(|previous| previous.contains(Op::RENAME))
        {
            self.superseded.insert(path.clone());
        }
        if op.contains(Op::RENAME) {
            if let Some(cookie) = cookie {
                let paths = self.renames.entry(cookie).or_default();
                if !paths.contains(&path) {
                    paths.push(path.clone());
                }
            }
        }
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
        let mut indexes = HashMap::new();

        for path in self.order {
            let op = ops.remove(&path).unwrap_or_else(Op::empty);
            let event = if !exists(&path) {
                VfsEvent::Remove(path.clone())
            } else if op.intersects(Op::CREATE | Op::REMOVE | Op::RENAME) {
                VfsEvent::Create(path.clone())
            } else if op.intersects(Op::WRITE | Op::CLOSE_WRITE) {
                VfsEvent::Write(path.clone())
            } else {
                continue;
            };
            indexes.insert(path, events.len());
            events.push(event);
        }

        // Cookies are linkage supplied by the native watcher. Multiple partners or
        // missing endpoints are ambiguous, so retain ordinary events in those cases.
        let mut partners = HashMap::new();
        for paths in self.renames.values() {
            for path in paths {
                *partners.entry(path).or_insert(0) += 1;
            }
        }
        let mut replacements = HashMap::new();
        let mut skipped = vec![false; events.len()];
        for paths in self.renames.values() {
            let [first, second] = paths.as_slice() else {
                continue;
            };
            if partners[first] != 1
                || partners[second] != 1
                || self.superseded.contains(first)
                || self.superseded.contains(second)
            {
                continue;
            }
            let (Some(&a), Some(&b)) = (indexes.get(first), indexes.get(second)) else {
                continue;
            };
            let (from, to) = match (&events[a], &events[b]) {
                (VfsEvent::Remove(from), VfsEvent::Create(to))
                | (VfsEvent::Create(to), VfsEvent::Remove(from)) => (from.clone(), to.clone()),
                _ => continue,
            };
            replacements.insert(
                a.min(b),
                VfsEvent::Rename {
                    from,
                    to,
                    identity: None,
                },
            );
            skipped[a.max(b)] = true;
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
            batch.record(PathBuf::from(path), *op, None);
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
                VfsEvent::Rename { from, to, .. } => {
                    format!("Rename {} to {}", from.display(), to.display())
                }
                VfsEvent::Batch(_) => panic!("Settling must return individual events."),
            })
            .collect()
    }

    #[test]
    fn only_matching_native_cookies_link_rename_paths() {
        for reverse in [false, true] {
            for linked in [false, true] {
                let mut records = vec![
                    ("/src/A", Op::RENAME, Some(10)),
                    ("/src/B", Op::RENAME, Some(if linked { 10 } else { 11 })),
                ];
                if reverse {
                    records.reverse();
                }
                let mut batch = WatchBatch::default();
                for (path, op, cookie) in records {
                    batch.record(path.into(), op, cookie);
                }
                let events = batch.settle(|path| path == Path::new("/src/B"));
                if linked {
                    assert_eq!(describe(&events), ["Rename /src/A to /src/B"]);
                    assert!(matches!(
                        &events[..],
                        [VfsEvent::Rename { identity: None, .. }]
                    ));
                } else {
                    assert_eq!(events.len(), 2);
                    assert!(!events
                        .iter()
                        .any(|event| matches!(event, VfsEvent::Rename { .. })));
                }
            }
        }
    }

    #[test]
    fn native_cookies_keep_simultaneous_identical_renames_distinct() {
        let mut batch = WatchBatch::default();
        for (path, cookie) in [("/a", 1), ("/b", 2), ("/y", 2), ("/z", 1)] {
            batch.record(path.into(), Op::RENAME, Some(cookie));
        }
        let events = batch.settle(|path| path == Path::new("/y") || path == Path::new("/z"));
        assert_eq!(describe(&events), ["Rename /a to /z", "Rename /b to /y"]);
    }

    #[test]
    fn replacing_an_endpoint_supersedes_its_earlier_rename_cookie() {
        let mut batch = WatchBatch::default();
        batch.record("/a".into(), Op::RENAME, Some(1));
        batch.record("/b".into(), Op::RENAME, Some(1));
        batch.record("/b".into(), Op::REMOVE, None);
        batch.record("/b".into(), Op::CREATE, None);
        let events = batch.settle(|path| path == Path::new("/b"));
        assert_eq!(describe(&events), ["Remove /a", "Create /b"]);
    }

    #[test]
    fn ambiguous_or_unlinked_paths_remain_independent_events() {
        for records in [
            vec![("/a", Some(1)), ("/b", Some(1)), ("/c", Some(1))],
            vec![("/a", None), ("/b", None)],
            vec![
                ("/a", Some(1)),
                ("/b", Some(1)),
                ("/a", Some(2)),
                ("/c", Some(2)),
            ],
        ] {
            let mut batch = WatchBatch::default();
            for (path, cookie) in records {
                batch.record(path.into(), Op::RENAME, cookie);
            }
            let events = batch.settle(|path| path != Path::new("/a"));
            assert!(!events
                .iter()
                .any(|event| matches!(event, VfsEvent::Rename { .. })));
        }
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
