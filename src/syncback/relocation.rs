use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::snapshot::{PatchUpdate, RojoTree};
use rbx_dom_weak::types::Ref;

use anyhow::{bail, Context};

use crate::snapshot_middleware::Middleware;

use super::{extension_for_middleware, validate_file_name};

// Sessions in the same process must not recover each other's active intents.
static RELOCATION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RelocationPlan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub sidecars: Vec<(PathBuf, PathBuf)>,
}

/// Plans paths only; the transaction checks their actual state before moving anything.
pub(crate) fn plan_relocation(
    source: &Path,
    middleware: Middleware,
    current_name: &str,
    requested_name: &str,
    destination_directory: &Path,
) -> anyhow::Result<RelocationPlan> {
    validate_file_name(requested_name)?;
    if matches!(middleware, Middleware::Project | Middleware::Ignore) {
        bail!("Project sources cannot be relocated from Studio");
    }
    let source_directory = source
        .parent()
        .context("The source has no parent directory")?;
    let filename = source
        .file_name()
        .and_then(|value| value.to_str())
        .context("The source filename is not valid Unicode")?;
    // Metadata can override the visible Name. Preserve the physical filename for
    // a Parent-only move rather than inadvertently renaming it as a side effect.
    let renamed = current_name != requested_name;
    let (destination_name, old_stem) = if middleware.is_dir() {
        (
            if renamed { requested_name } else { filename }.to_owned(),
            filename,
        )
    } else {
        let extension = match middleware {
            Middleware::ServerScript
            | Middleware::ClientScript
            | Middleware::ModuleScript
            | Middleware::PluginScript
            | Middleware::LegacyServerScript
            | Middleware::LegacyClientScript
            | Middleware::RunContextServerScript
            | Middleware::RunContextClientScript => script_suffix(filename)
                .context("Unsupported script filename")?
                .to_owned(),
            Middleware::Csv
            | Middleware::JsonModel
            | Middleware::Json
            | Middleware::Rbxm
            | Middleware::Rbxmx
            | Middleware::Toml
            | Middleware::Text
            | Middleware::Yaml => {
                if middleware == Middleware::Json && filename.ends_with(".jsonc") {
                    ".jsonc".to_owned()
                } else if middleware == Middleware::Yaml && filename.ends_with(".yaml") {
                    ".yaml".to_owned()
                } else {
                    format!(".{}", extension_for_middleware(middleware))
                }
            }
            _ => bail!("This source middleware cannot be relocated"),
        };
        let stem = filename
            .strip_suffix(&extension)
            .context("The source filename does not match its middleware")?;
        let new_filename = if renamed {
            // Dots can select a different script middleware on readback.
            if script_suffix(filename).is_some() && requested_name.contains('.') {
                bail!("Script names containing dots cannot be safely mapped to filenames");
            }
            format!("{requested_name}{extension}")
        } else {
            filename.to_owned()
        };
        (new_filename, stem)
    };
    if !middleware.is_dir()
        && crate::snapshot_middleware::dir_init_files()
            .iter()
            .any(|(_, initializer)| *initializer == destination_name)
    {
        bail!("A relocated child cannot become its parent's initializer");
    }
    let destination = destination_directory.join(destination_name);
    if middleware.is_dir() && destination.starts_with(source) && destination != source {
        bail!("A directory cannot be moved inside itself");
    }
    let sidecars = if middleware.is_dir() {
        Vec::new()
    } else {
        let destination_stem = if renamed { requested_name } else { old_stem };
        ["meta.json", "meta.jsonc"]
            .into_iter()
            .map(|extension| {
                (
                    source_directory.join(format!("{old_stem}.{extension}")),
                    destination_directory.join(format!("{destination_stem}.{extension}")),
                )
            })
            .collect()
    };
    Ok(RelocationPlan {
        source: source.to_path_buf(),
        destination,
        sidecars,
    })
}

pub(crate) fn cache_directory(project: &crate::Project) -> PathBuf {
    project.folder_location().join(".rojo").join("relocations")
}

/// Where a relayout sets an instance's old files aside while it writes the new
/// layout. Startup recovery puts back whatever an interrupted one left here.
pub(crate) fn relayout_directory(project: &crate::Project) -> PathBuf {
    project.folder_location().join(".rojo").join("relayout")
}

/// What a relayout set aside, written before anything moves, so recovery can
/// put each file back where it came from.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct RelayoutManifest {
    /// Each original path, with the name its backup has in the same folder as
    /// this manifest.
    pub entries: Vec<(PathBuf, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<RelayoutPackage>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct RelayoutPackage {
    version: u32,
    ownership: super::relocation_transaction::ReplacementOwnership,
    originals: Vec<super::relocation_transaction::Relocation>,
}

impl RelayoutManifest {
    pub(crate) fn prepare(
        vfs: &memofs::Vfs,
        directory: &Path,
        paths: &[PathBuf],
        destination_directory: &Path,
    ) -> std::io::Result<Self> {
        let originals = paths
            .iter()
            .map(|path| {
                Ok(super::relocation_transaction::Relocation {
                    source: path.clone(),
                    destination: path.clone(),
                    expected: vfs.snapshot_tree(path)?,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let package = super::relocation_transaction::capture_structural_ownership(
            vfs, directory, &originals,
        )?
        .map(|ownership| {
            // Relayout writes only children of this container. Individual protected
            // files are still checked by the serializer's write-target guard.
            let originals = originals
                .into_iter()
                .map(|mut entry| {
                    entry.destination = destination_directory.to_path_buf();
                    entry
                })
                .collect();
            RelayoutPackage {
                version: 2,
                ownership,
                originals,
            }
        });
        let manifest = Self {
            entries: paths
                .iter()
                .enumerate()
                .map(|(index, path)| (path.clone(), index.to_string()))
                .collect(),
            package,
        };
        manifest.validate_package(vfs, directory)?;
        Ok(manifest)
    }

    pub(crate) fn expected_original(&self, index: usize) -> Option<&memofs::VfsSnapshot> {
        self.package
            .as_ref()?
            .originals
            .get(index)
            .map(|entry| &entry.expected)
    }

    pub(crate) fn validate_package(
        &self,
        vfs: &memofs::Vfs,
        directory: &Path,
    ) -> std::io::Result<()> {
        let Some(package) = &self.package else {
            // Legacy records can only be checked against current declarations.
            // Removed historical declarations are not reconstructible from paths.
            let originals: Vec<_> = self
                .entries
                .iter()
                .map(|(path, _)| super::relocation_transaction::Relocation {
                    source: path.clone(),
                    destination: path.clone(),
                    expected: memofs::VfsSnapshot::file(Vec::new()),
                })
                .collect();
            if super::relocation_transaction::capture_structural_ownership(
                vfs, directory, &originals,
            )?
            .is_some()
            {
                return Err(std::io::Error::other("Legacy relayout has no durable package ownership witness. Journal and backups retained"));
            }
            return Ok(());
        };
        if package.version != 2
            || self.entries.len() != package.originals.len()
            || self.entries.iter().zip(&package.originals).enumerate().any(
                |(index, ((path, backup), entry))| {
                    path != &entry.source || backup != &index.to_string()
                },
            )
        {
            return Err(std::io::Error::other("Invalid package relayout evidence"));
        }
        let mut fs = vfs.lock();
        super::relocation_transaction::validate_structural_ownership(
            &mut fs,
            directory,
            Some(&package.ownership),
            &package.originals,
        )?;
        for (index, entry) in package.originals.iter().enumerate() {
            super::relocation_transaction::require_canonical_parent(
                &mut fs,
                &entry.destination.join(".relayout-output"),
            )?;
            use memofs::IoResultExt;
            let backup = fs
                .snapshot_tree(&directory.join(index.to_string()))
                .with_not_found()?;
            let valid = match backup {
                Some(actual) => actual == entry.expected,
                None => {
                    fs.snapshot_tree(&entry.source).with_not_found()?.as_ref()
                        == Some(&entry.expected)
                }
            };
            if !valid {
                return Err(std::io::Error::other(
                    "Package relayout original or backup changed or disappeared. Journal and backups retained",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) const RELAYOUT_MANIFEST_NAME: &str = "manifest.json";

// Relayouts in the same process, and their recovery, must not overlap.
static RELAYOUT_LOCK: Mutex<()> = Mutex::new(());

/// Held for the whole of a relayout, and by recovery while it reads the
/// folders relayouts leave, so neither sees the other halfway through, even
/// across server processes.
pub(crate) struct RelayoutGuard {
    _file: memofs::VfsLockGuard,
    _process: std::sync::MutexGuard<'static, ()>,
}

pub(crate) fn lock_relayouts(
    vfs: &memofs::Vfs,
    project: &crate::Project,
) -> std::io::Result<RelayoutGuard> {
    let process = RELAYOUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = relayout_directory(project);
    vfs.create_dir_all(&root)?;
    let file = vfs.lock_exclusive(&root.with_extension("lock"))?;
    Ok(RelayoutGuard {
        _file: file,
        _process: process,
    })
}

/// Puts back the files of relayouts that were interrupted, before anything is
/// read. A backup whose original path is taken again is kept, and so is a
/// folder without a readable manifest, since neither can be put back safely.
pub(crate) fn recover_relayouts(
    vfs: &memofs::Vfs,
    project: &crate::Project,
) -> std::io::Result<()> {
    use memofs::IoResultExt;
    let root = relayout_directory(project);
    // Read-only serving must not create a lock when there's nothing to do.
    if vfs.read_dir(&root).with_not_found()?.is_none() {
        return Ok(());
    }
    // Validate every package journal before creating a lock or restoring any batch.
    // Repeat under the lock below because another server may have changed the cache.
    for folder in vfs.read_dir(&root)? {
        let folder = folder?.path().to_path_buf();
        if let Some(bytes) = vfs
            .read(folder.join(RELAYOUT_MANIFEST_NAME))
            .with_not_found()?
        {
            if let Ok(manifest) = serde_json::from_slice::<RelayoutManifest>(&bytes) {
                manifest.validate_package(vfs, &folder)?;
            }
        }
    }
    let _guard = lock_relayouts(vfs, project)?;
    let Some(folders) = vfs.read_dir(&root).with_not_found()? else {
        return Ok(());
    };
    let folders = folders.collect::<std::io::Result<Vec<_>>>()?;
    for folder in folders {
        let folder = folder.path().to_path_buf();
        let manifest = vfs
            .read(folder.join(RELAYOUT_MANIFEST_NAME))
            .with_not_found()?
            .and_then(|manifest| serde_json::from_slice::<RelayoutManifest>(&manifest).ok());
        let Some(manifest) = manifest else {
            log::warn!(
                "Kept \"{}\", which an interrupted relayout left without a readable manifest.",
                folder.display()
            );
            continue;
        };
        manifest.validate_package(vfs, &folder)?;
        let mut restored_all = true;
        for (original, backup) in manifest.entries {
            let backup = folder.join(backup);
            if vfs.metadata(&backup).with_not_found()?.is_none() {
                continue;
            }
            if vfs.metadata(&original).with_not_found()?.is_some() {
                log::warn!(
                    "Kept \"{}\", since \"{}\" exists again.",
                    backup.display(),
                    original.display()
                );
                restored_all = false;
                continue;
            }
            let expected = vfs.snapshot_tree(&backup)?;
            vfs.move_no_replace(&backup, &original, &expected)?;
        }
        if restored_all {
            vfs.remove_dir_all(&folder)?;
            log::warn!(
                "Undid a relayout that was interrupted. If it had written the new layout already, the folder may hold the instance twice."
            );
        }
    }
    Ok(())
}

/// Where new files are written before a relocation moves them into the
/// project. It's kept apart from the journals, which recovery reads.
pub(crate) fn staging_directory(project: &crate::Project) -> PathBuf {
    project.folder_location().join(".rojo").join("staging")
}

/// Like [`execute_relocation`], with the moves made by `stage` while the
/// relocation lock is held, so recovery never sees a staging directory that's
/// still being written.
pub(crate) fn execute_staged_relocation(
    vfs: &memofs::Vfs,
    journal: &Path,
    stage: impl FnOnce() -> std::io::Result<Vec<super::relocation_transaction::Relocation>>,
) -> std::io::Result<()> {
    execute_transaction(vfs, journal, || {
        let moves = stage()?;
        super::relocation_transaction::execute_staged(vfs, journal, moves)
    })
}

/// Retire terminal intents before subsequent moves can make their old parents vanish.
/// An unresolved intent is deliberately retained for startup recovery.
pub(crate) fn execute_relocation(
    vfs: &memofs::Vfs,
    journal: &Path,
    moves: Vec<super::relocation_transaction::Relocation>,
    absent_paths: Vec<PathBuf>,
) -> std::io::Result<()> {
    execute_transaction(vfs, journal, || {
        super::relocation_transaction::execute(vfs, journal, moves, absent_paths)
    })
}

pub(crate) fn execute_promotion(
    vfs: &memofs::Vfs,
    journal: &Path,
    promotion: super::relocation_transaction::Promotion,
    absent_paths: Vec<PathBuf>,
) -> std::io::Result<()> {
    execute_transaction(vfs, journal, || {
        super::relocation_transaction::execute_promotion(vfs, journal, promotion, absent_paths)
    })
}

pub(crate) fn execute_replacements(
    vfs: &memofs::Vfs,
    journal: &Path,
    entries: Vec<super::relocation_transaction::Replacement>,
) -> std::io::Result<()> {
    execute_transaction(vfs, journal, || {
        super::relocation_transaction::execute_replacements(vfs, journal, entries)
    })
}

pub(crate) fn execute_script_replacements(
    vfs: &memofs::Vfs,
    journal: &Path,
    project: &crate::Project,
    entries: Vec<super::relocation_transaction::Replacement>,
    expected: &HashMap<PathBuf, Option<Vec<u8>>>,
    ownership: &super::ScriptMigrationOwnership,
) -> std::io::Result<()> {
    execute_transaction(vfs, journal, || {
        super::relocation_transaction::execute_script_replacements_guarded(
            vfs,
            journal,
            project,
            entries,
            expected,
            Some(ownership),
        )
    })
}

fn execute_transaction(
    vfs: &memofs::Vfs,
    journal: &Path,
    execute: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let _guard = RELOCATION_LOCK.lock().unwrap();
    let cache = journal
        .parent()
        .ok_or_else(|| std::io::Error::other("A relocation journal must have a cache directory"))?;
    let lock = relocation_lock_path(cache)?;
    let _filesystem_guard = vfs.lock_exclusive(&lock)?;
    // Make creation durable under exclusion even if a caller already made the cache.
    // No intent is published until both directory entries have been flushed.
    vfs.create_dir_all(cache)?;
    vfs.sync_path(cache.parent().unwrap())?;
    vfs.sync_path(cache)?;
    let result = execute();
    // A successful execute has already flushed its commit marker. Recovery reparses
    // the whole journal, which holds every moved file's bytes, so only failures and
    // journals without a visible commit need it.
    let committed = result.is_ok() && vfs.metadata(journal.join("commit")).is_ok();
    if committed || super::relocation_transaction::recover(vfs, journal).is_ok() {
        if let Err(error) = retire_journal(vfs, journal) {
            log::warn!(
                "Could not retire relocation journal \"{}\" ({error}).",
                journal.display()
            );
        }
    }
    result
}

fn relocation_lock_path(cache: &Path) -> std::io::Result<PathBuf> {
    cache
        .parent()
        .map(|parent| parent.join("relocations.lock"))
        .ok_or_else(|| std::io::Error::other("A relocation cache must have a parent directory"))
}

fn retire_journal(vfs: &memofs::Vfs, journal: &Path) -> std::io::Result<()> {
    let cache = journal.parent().unwrap();
    let retired = cache.join(format!(".retired-{}", uuid::Uuid::new_v4()));
    // Renaming before deletion makes even interrupted recursive cleanup distinguishable
    // from a corrupt active intent. Do not remove any bytes until the rename is durable.
    let expected = vfs.snapshot_tree(journal)?;
    vfs.move_no_replace(journal, &retired, &expected)?;
    vfs.sync_path(cache)?;
    vfs.remove_dir_all(&retired)?;
    vfs.sync_path(cache)
}

/// Recovery must precede snapshots, including when no Studio client reconnects.
pub(crate) fn recover_relocations(
    vfs: &memofs::Vfs,
    project: &crate::Project,
) -> std::io::Result<()> {
    use memofs::IoResultExt;
    let _guard = RELOCATION_LOCK.lock().unwrap();
    let cache = cache_directory(project);
    let staging = staging_directory(project);
    let has_entries = |directory: &Path| -> std::io::Result<bool> {
        match vfs.read_dir(directory).with_not_found()? {
            Some(mut entries) => entries.next().transpose().map(|entry| entry.is_some()),
            None => Ok(false),
        }
    };
    // Read-only serving must not create a lock or require a writable cache when
    // there is nothing to recover. This observation is not used for recovery.
    if !has_entries(&cache)? && !has_entries(&staging)? {
        return Ok(());
    }
    let _filesystem_guard = vfs.lock_exclusive(&relocation_lock_path(&cache)?)?;
    // Journals can still name staged files, so staging is only cleared once
    // they're recovered. Staging happens under this lock, so nothing is
    // being written there now.
    recover_journals(vfs, &cache)?;
    if vfs.read_dir(&staging).with_not_found()?.is_some() {
        vfs.remove_dir_all(&staging)?;
    }
    Ok(())
}

fn recover_journals(vfs: &memofs::Vfs, cache: &Path) -> std::io::Result<()> {
    use memofs::IoResultExt;
    // Re-enumerate only after acquiring exclusion. An executing process may have
    // completed and retired every entry observed before we waited for its lock.
    let Some(entries) = vfs.read_dir(cache).with_not_found()? else {
        return Ok(());
    };
    let cache = cache.to_path_buf();
    let entries = entries.collect::<std::io::Result<Vec<_>>>()?;
    // StdBackend rejects linked cache roots when flushing, before recovery can
    // mutate a journal reached through a different coordinator's lock domain.
    vfs.sync_path(&cache)?;
    for entry in entries {
        let preparing = entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(".preparing-"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok());
        if preparing {
            if super::relocation_transaction::preparation_is_discardable(vfs, entry.path())? {
                retire_journal(vfs, entry.path())?;
            } else {
                log::warn!(
                    "Preserved incomplete relocation preparation at \"{}\".",
                    entry.path().display()
                );
            }
            continue;
        }
        let retired = entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(".retired-"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok());
        if retired {
            vfs.remove_dir_all(entry.path())?;
            vfs.sync_path(&cache)?;
        } else {
            if super::relocation_transaction::rolls_back_outside_project(vfs, entry.path())? {
                return Err(std::io::Error::other(format!(
                    "Refused to recover the script rename journal at \"{}\" because it would write outside its project folder. Check the listed files, then delete the journal folder to continue.",
                    entry.path().display()
                )));
            }
            super::relocation_transaction::recover(vfs, entry.path())?;
            retire_journal(vfs, entry.path())?;
        }
    }
    Ok(())
}

/// Validate the final graph before writing any files, then simulate safe intermediate
/// graphs. A valid ancestor reversal must move the old descendant out first.
pub(crate) fn relocation_order(
    tree: &RojoTree,
    updates: &[PatchUpdate],
    removed: &[Ref],
) -> anyhow::Result<Vec<usize>> {
    if updates.iter().all(|update| update.changed_parent.is_none()) {
        return Ok((0..updates.len()).collect());
    }
    let removed: HashSet<_> = removed.iter().copied().collect();
    let mut parents: HashMap<_, _> = tree
        .descendants(tree.get_root_id())
        .map(|instance| (instance.id(), instance.parent()))
        .collect();
    let mut final_parents = parents.clone();
    let mut edited = HashSet::new();
    for update in updates {
        if let Some(parent) = update.changed_parent {
            anyhow::ensure!(
                update.id != tree.get_root_id(),
                "Cannot relocate the tree root"
            );
            anyhow::ensure!(
                parents.contains_key(&update.id),
                "Cannot relocate an unknown instance"
            );
            anyhow::ensure!(
                parents.contains_key(&parent),
                "The destination parent does not exist"
            );
            anyhow::ensure!(
                edited.insert(update.id),
                "A batch cannot reparent an instance twice"
            );
            final_parents.insert(update.id, parent);
        }
    }
    for update in updates
        .iter()
        .filter(|update| update.changed_parent.is_some())
    {
        let mut seen = HashSet::new();
        let mut ancestor = update.id;
        while ancestor.is_some() {
            anyhow::ensure!(
                seen.insert(ancestor),
                "The batch's final parent graph contains a cycle"
            );
            anyhow::ensure!(
                !removed.contains(&ancestor),
                "The relocation involves a removed subtree"
            );
            ancestor = *final_parents
                .get(&ancestor)
                .context("Unknown ancestor in parent graph")?;
        }
    }
    let mut pending: Vec<_> = (0..updates.len()).collect();
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let next = pending
            .iter()
            .position(|&index| {
                let update = &updates[index];
                let Some(mut ancestor) = update.changed_parent else {
                    return true;
                };
                while ancestor.is_some() {
                    if ancestor == update.id {
                        return false;
                    }
                    ancestor = parents[&ancestor];
                }
                true
            })
            .context("No safe intermediate relocation order exists")?;
        let index = pending.remove(next);
        if let Some(parent) = updates[index].changed_parent {
            parents.insert(updates[index].id, parent);
        }
        ordered.push(index);
    }
    Ok(ordered)
}

fn script_suffix(filename: &str) -> Option<&str> {
    [
        ".server.luau",
        ".client.luau",
        ".plugin.luau",
        ".legacy.luau",
        ".local.luau",
        ".legacy.lua",
        ".local.lua",
        ".server.lua",
        ".client.lua",
        ".plugin.lua",
        ".luau",
        ".lua",
    ]
    .into_iter()
    .find(|suffix| filename.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_parent_updates_keep_their_original_order_without_graph_validation() {
        let tree = RojoTree::new(crate::snapshot::InstanceSnapshot::new());
        let updates: Vec<_> = (0..3)
            .map(|_| PatchUpdate {
                id: Ref::new(),
                changed_name: Some("Renamed".into()),
                changed_class_name: None,
                changed_parent: None,
                changed_properties: Default::default(),
                changed_metadata: None,
            })
            .collect();
        assert_eq!(
            relocation_order(&tree, &updates, &[]).unwrap(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn moves_legacy_scripts_with_both_possible_sidecars() {
        let plan = plan_relocation(
            Path::new("src/Before.server.lua"),
            Middleware::ServerScript,
            "Before",
            "After",
            Path::new("dst"),
        )
        .unwrap();
        assert_eq!(plan.destination, Path::new("dst/After.server.lua"));
        assert_eq!(
            plan.sidecars,
            vec![
                ("src/Before.meta.json".into(), "dst/After.meta.json".into()),
                (
                    "src/Before.meta.jsonc".into(),
                    "dst/After.meta.jsonc".into()
                ),
            ]
        );
    }

    #[test]
    fn parent_only_moves_preserve_metadata_overridden_filenames() {
        let plan = plan_relocation(
            Path::new("src/Physical.luau"),
            Middleware::ModuleScript,
            "Visible",
            "Visible",
            Path::new("dst"),
        )
        .unwrap();
        assert_eq!(plan.destination, Path::new("dst/Physical.luau"));
        assert_eq!(plan.sidecars[0].1, Path::new("dst/Physical.meta.json"));
    }

    #[test]
    fn directory_moves_do_not_reconstruct_contents() {
        let plan = plan_relocation(
            Path::new("src/Before"),
            Middleware::ModuleScriptDir,
            "Before",
            "After",
            Path::new("dst"),
        )
        .unwrap();
        assert_eq!(plan.destination, Path::new("dst/After"));
        assert!(plan.sidecars.is_empty());
    }

    #[test]
    fn rejects_invalid_or_semantically_ambiguous_names() {
        for name in ["", ".", "..", "a/b", "a\\b", "a\0b", "Foo.server"] {
            assert!(
                plan_relocation(
                    Path::new("src/Foo.luau"),
                    Middleware::ModuleScript,
                    "Foo",
                    name,
                    Path::new("dst")
                )
                .is_err(),
                "{name:?}"
            );
        }
        assert!(plan_relocation(
            Path::new("src/Folder"),
            Middleware::Dir,
            "Folder",
            "Folder",
            Path::new("src/Folder/Child")
        )
        .is_err());
    }

    #[test]
    fn metadata_name_prefixes_do_not_change_the_physical_sidecar_stem() {
        let plan = plan_relocation(
            Path::new("src/Physical.extra.server.lua"),
            Middleware::ServerScript,
            "Physical",
            "Renamed",
            Path::new("dst"),
        )
        .unwrap();
        assert_eq!(plan.destination, Path::new("dst/Renamed.server.lua"));
        assert_eq!(
            plan.sidecars[0].0,
            Path::new("src/Physical.extra.meta.json")
        );
    }

    #[test]
    fn alternate_spelling_is_preserved_and_initializer_names_are_rejected() {
        for (filename, middleware, destination) in [
            ("Before.jsonc", Middleware::Json, "dst/After.jsonc"),
            ("Before.yml", Middleware::Yaml, "dst/After.yml"),
            ("Before.yaml", Middleware::Yaml, "dst/After.yaml"),
            (
                "Before.legacy.lua",
                Middleware::LegacyServerScript,
                "dst/After.legacy.lua",
            ),
        ] {
            let plan = plan_relocation(
                &Path::new("src").join(filename),
                middleware,
                "Before",
                "After",
                Path::new("dst"),
            )
            .unwrap();
            assert_eq!(plan.destination, Path::new(destination));
        }
        assert!(plan_relocation(
            Path::new("src/Before.luau"),
            Middleware::ModuleScript,
            "Before",
            "init",
            Path::new("dst")
        )
        .is_err());
    }

    #[test]
    fn execution_locks_before_journaling_and_keeps_the_lock_outside_retirement() {
        use super::super::relocation_transaction::Relocation;

        for obstructed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let source = root.join("source");
            let destination = root.join("destination");
            let cache = root.join(".rojo/relocations");
            let lock = relocation_lock_path(&cache).unwrap();
            let journal = cache.join("transaction");
            std::fs::write(&source, b"contents").unwrap();
            if obstructed {
                std::fs::create_dir_all(&lock).unwrap();
            }
            let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let result = execute_relocation(
                &vfs,
                &journal,
                vec![Relocation {
                    source: source.clone(),
                    destination: destination.clone(),
                    expected: vfs.snapshot_tree(&source).unwrap(),
                }],
                Vec::new(),
            );
            if obstructed {
                assert!(result.is_err());
                assert!(!cache.exists());
                assert_eq!(std::fs::read(&source).unwrap(), b"contents");
                assert!(!destination.exists());
            } else {
                result.unwrap();
                assert!(!source.exists());
                assert_eq!(std::fs::read(&destination).unwrap(), b"contents");
                assert!(lock.is_file());
                assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn promotion_uses_the_same_lock_and_retirement_boundary() {
        use super::super::relocation_transaction::{Promotion, Relocation};
        for obstructed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let source = root.join("script.lua");
            let destination = root.join("script");
            let cache = root.join(".rojo/relocations");
            let lock = relocation_lock_path(&cache).unwrap();
            let journal = cache.join("promotion");
            std::fs::write(&source, b"return 1").unwrap();
            if obstructed {
                std::fs::create_dir_all(&lock).unwrap();
            }
            let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let result = execute_promotion(
                &vfs,
                &journal,
                Promotion {
                    destination: destination.clone(),
                    originals: vec![Relocation {
                        source: source.clone(),
                        destination: destination.join("init.lua"),
                        expected: vfs.snapshot_tree(&source).unwrap(),
                    }],
                },
                Vec::new(),
            );
            if obstructed {
                assert!(result.is_err());
                assert!(!cache.exists());
                assert_eq!(std::fs::read(&source).unwrap(), b"return 1");
                assert!(!destination.exists());
            } else {
                result.unwrap();
                assert!(!source.exists());
                assert_eq!(
                    std::fs::read(destination.join("init.lua")).unwrap(),
                    b"return 1"
                );
                assert!(lock.is_file());
                assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn recovery_lock_subprocess_helper() {
        use std::io::{Read, Write};
        let Some(root) = std::env::var_os("ROJO_RECOVERY_LOCK_TEST_ROOT") else {
            return;
        };
        let root = Path::new(&root);
        let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let cache = root.join(".rojo/relocations");
        let _guard = vfs
            .lock_exclusive(&relocation_lock_path(&cache).unwrap())
            .unwrap();
        // Model the execute boundary while its intent is not yet complete. A
        // recovery scan without exclusion would reject this directory as corrupt.
        let journal = cache.join("active");
        vfs.create_dir_all(&journal).unwrap();
        println!("Active intent.");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read(&mut [0]);
        vfs.remove_dir_all(&journal).unwrap();
        vfs.sync_path(&cache).unwrap();
    }

    #[test]
    fn recovery_waits_for_active_process_and_rescans_after_retirement() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path().join("default.project.json");
        std::fs::write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let vfs = Arc::new(memofs::Vfs::new(memofs::StdBackend::new().unwrap()));
        vfs.set_watch_enabled(false);
        let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "syncback::relocation::tests::recovery_lock_subprocess_helper",
                "--nocapture",
            ])
            .env("ROJO_RECOVERY_LOCK_TEST_ROOT", temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if line.trim() == "Active intent." {
                break;
            }
        }
        let (tx, rx) = mpsc::channel();
        let recovering = std::thread::spawn(move || {
            tx.send(recover_relocations(&vfs, &project)).unwrap();
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(child.wait().unwrap().success());
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        recovering.join().unwrap();
        assert_eq!(
            std::fs::read_dir(temp.path().join(".rojo/relocations"))
                .unwrap()
                .count(),
            0
        );
    }

    /// Files staged by a write that never finished are cleared at startup, and
    /// recovery never mistakes them for a journal.
    /// Files an interrupted relayout set aside go back where they came from,
    /// whether they were a file or a folder. A backup whose original path is
    /// taken again is kept, and so is a folder without a manifest.
    #[test]
    fn recovery_puts_back_what_a_relayout_set_aside() {
        for taken in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let project_path = temp.path().join("default.project.json");
            std::fs::write(
                &project_path,
                r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#,
            )
            .unwrap();
            std::fs::create_dir_all(temp.path().join("src")).unwrap();
            let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();

            let aside = relayout_directory(&project).join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(aside.join("0")).unwrap();
            std::fs::write(aside.join("0/init.luau"), "return 'folder'").unwrap();
            std::fs::write(aside.join("1"), "{}").unwrap();
            let folder = temp.path().join("src/Mover");
            let sidecar = temp.path().join("src/Mover.meta.json");
            let manifest = RelayoutManifest {
                entries: vec![(folder.clone(), "0".into()), (sidecar.clone(), "1".into())],
                package: None,
            };
            std::fs::write(
                aside.join(RELAYOUT_MANIFEST_NAME),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            if taken {
                std::fs::write(&sidecar, "taken").unwrap();
            }
            let orphan = relayout_directory(&project).join("orphan");
            std::fs::create_dir_all(&orphan).unwrap();

            recover_relayouts(&vfs, &project).unwrap();

            assert_eq!(
                std::fs::read_to_string(folder.join("init.luau")).unwrap(),
                "return 'folder'"
            );
            if taken {
                assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "taken");
                assert!(aside.join("1").exists());
            } else {
                assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "{}");
                assert!(!aside.exists());
            }
            assert!(orphan.exists());
        }
    }

    #[test]
    fn recovery_clears_leftover_staging() {
        for with_journal_cache in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let project_path = temp.path().join("default.project.json");
            std::fs::write(
                &project_path,
                r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
            )
            .unwrap();
            let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
            if with_journal_cache {
                std::fs::create_dir_all(cache_directory(&project)).unwrap();
            }
            let staged = staging_directory(&project).join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(staged.join("New")).unwrap();
            std::fs::write(staged.join("New/init.meta.json"), "{}").unwrap();

            recover_relocations(&vfs, &project).unwrap();

            assert!(!staging_directory(&project).exists());
        }
    }

    #[test]
    fn recovery_without_journals_never_creates_a_lock_file() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path().join("default.project.json");
        std::fs::write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        recover_relocations(&vfs, &project).unwrap();
        assert!(!temp.path().join(".rojo").exists());
        let cache = cache_directory(&project);
        std::fs::create_dir_all(&cache).unwrap();
        let original_permissions = std::fs::metadata(&cache).unwrap().permissions();
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&cache, permissions).unwrap();
        let result = recover_relocations(&vfs, &project);
        // Restore before the temporary directory is cleaned up, also on Windows.
        std::fs::set_permissions(&cache, original_permissions).unwrap();
        result.unwrap();
        assert!(!relocation_lock_path(&cache).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_linked_cache_before_retiring_anything() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path().join("default.project.json");
        std::fs::write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let other_cache = temp.path().join("other-cache");
        let retired = other_cache.join(format!(".retired-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&retired).unwrap();
        std::fs::write(retired.join("sentinel"), "keep").unwrap();
        std::fs::create_dir(temp.path().join(".rojo")).unwrap();
        std::os::unix::fs::symlink(&other_cache, cache_directory(&project)).unwrap();
        assert!(recover_relocations(&vfs, &project).is_err());
        assert_eq!(std::fs::read(retired.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn partially_deleted_retired_journals_do_not_block_recovery() {
        use memofs::{InMemoryFs, Vfs, VfsSnapshot};
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot(
                "/project",
                VfsSnapshot::dir([
                    (
                        "default.project.json",
                        VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
                    ),
                    ("Code.luau", VfsSnapshot::file("return 1")),
                ]),
            )
            .unwrap();
        let vfs = Vfs::new(backend);
        vfs.set_watch_enabled(false);
        let project = crate::Project::load_exact(
            &vfs,
            Path::new("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let retired = cache_directory(&project).join(format!(".retired-{}", uuid::Uuid::new_v4()));
        vfs.create_dir_all(&retired).unwrap();
        vfs.write(retired.join("commit"), "committed\n").unwrap();
        recover_relocations(&vfs, &project).unwrap();
        assert!(vfs.metadata(retired).is_err());
        assert_eq!(
            vfs.read_to_string("/project/Code.luau").unwrap().as_str(),
            "return 1"
        );
    }

    #[test]
    fn planning_is_deterministic() {
        let plan = || {
            plan_relocation(
                Path::new("src/Before.txt"),
                Middleware::Text,
                "Before",
                "After",
                Path::new("dst"),
            )
            .unwrap()
        };
        assert_eq!(plan(), plan());
    }
}
