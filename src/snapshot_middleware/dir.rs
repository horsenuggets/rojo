use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::Arc,
};

use anyhow::Context;
use memofs::{DirEntry, IoResultExt as _, Vfs};
use rayon::prelude::*;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Ustr,
};

use crate::{
    snapshot::{
        is_path_ignored, InstanceContext, InstanceMetadata, InstanceSnapshot, InstanceWithMeta,
        InstigatingSource,
    },
    syncback::{
        directory_middleware_for_instance, extension_for_middleware, filter_properties,
        hash_instance, instances_as_directories, is_valid_path, sibling_order_key,
        unique_child_stems, FsSnapshot, SyncbackReturn, SyncbackSnapshot,
    },
    REF_ID_ATTRIBUTE_NAME,
};

use super::{
    meta_file::{serialize_values, DirectoryMetadata, InlineInstance},
    snapshot_from_vfs,
};

const EMPTY_DIR_KEEP_NAME: &str = ".gitkeep";

pub fn snapshot_dir(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
    entries: Option<Vec<DirEntry>>,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let mut snapshot = match snapshot_dir_no_meta(context, vfs, path, name, entries)? {
        Some(snapshot) => snapshot,
        None => return Ok(None),
    };

    DirectoryMetadata::read_and_apply_all(vfs, path, &mut snapshot)?;

    Ok(Some(snapshot))
}

/// Snapshot a directory without applying meta files; useful for if the
/// directory's ClassName will change before metadata should be applied. For
/// example, this can happen if the directory contains an `init.client.lua`
/// file.
pub fn snapshot_dir_no_meta(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
    entries: Option<Vec<DirEntry>>,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let normalized_path = vfs.canonicalize(path)?;
    let mut context = context.clone();
    let active_directories = context
        .active_directories
        .get_or_insert_with(|| Arc::new(Vec::new()));
    if active_directories.contains(&normalized_path) {
        anyhow::bail!(
            "Directory dependency cycle includes \"{}\".",
            path.display()
        );
    }
    Arc::make_mut(active_directories).push(normalized_path.clone());

    let entries = match entries {
        // A shallow read only wants this directory's own metadata.
        _ if context.shallow => Vec::new(),
        Some(entries) => entries,
        None => vfs.read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?,
    };
    let mut entries: Vec<_> = entries
        .into_iter()
        .filter(|child| !is_path_ignored(&context.path_ignore_rules, child.path()))
        .collect();
    // Restore the original order of duplicate-named siblings, which syncback
    // numbers `~2`, `~3`, ... and a plain listing would put `~10` before `~2`.
    entries.sort_by_cached_key(|entry| {
        let name = entry
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (base, suffix) = sibling_order_key(&name);
        (base.into_owned(), suffix)
    });
    let snapshot_child = |entry: &DirEntry| snapshot_from_vfs(&context, vfs, entry.path());
    // Preserve filesystem ordering, including which error is reported first.
    // Small directories keep the original short-circuiting traversal.
    let children: Vec<_> = if entries.len() >= 8 && rayon::current_num_threads() > 1 {
        entries
            .par_iter()
            .map(snapshot_child)
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<anyhow::Result<_>>()?
    } else {
        entries
            .iter()
            .map(snapshot_child)
            .collect::<anyhow::Result<_>>()?
    };
    let snapshot_children: Vec<_> = children.into_iter().flatten().collect();

    // Init files that don't exist yet are not listed. Change processing walks up
    // from an unknown path to the nearest tracked ancestor, so creating one still
    // resnapshots this directory, and every listed path costs memory for each
    // directory in the tree.
    let relevant_paths = vec![normalized_path.clone()];

    let snapshot = InstanceSnapshot::new()
        .name(name)
        .class_name("Folder")
        .children(snapshot_children)
        .metadata(
            InstanceMetadata::new()
                .instigating_source(path)
                .relevant_paths(relevant_paths)
                .context(&context),
        );

    Ok(Some(snapshot))
}

pub fn syncback_dir<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();

    let mut meta = DirectoryMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    // Inline children only exist in the metadata, so they need a file to land in.
    let inline = meta.is_some() && should_inline(snapshot);
    let mut dir_syncback = if inline {
        syncback_dir_inline(snapshot)
    } else {
        syncback_dir_no_meta(snapshot)?
    };

    let is_file = |path: &Path| -> anyhow::Result<bool> {
        Ok(snapshot
            .vfs()
            .metadata(path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_file()))
    };
    let json_path = snapshot.path.join("init.meta.json");
    let jsonc_path = snapshot.path.join("init.meta.jsonc");
    // An existing JSONC file is rewritten in place. A second init.meta.json next
    // to it would be read too and load its inline children twice.
    let meta_path = if is_file(&jsonc_path)? && !is_file(&json_path)? {
        jsonc_path.clone()
    } else {
        json_path
    };
    let was_inline = has_inline_children(snapshot);

    let mut wrote_meta = false;
    if let Some(meta) = &mut meta {
        if new_inst.class != "Folder" {
            meta.class_name = Some(new_inst.class);
        }
        if inline {
            meta.children = inline_children(snapshot, snapshot.new, snapshot.old);
        }

        // Metadata that used to hold inline children is rewritten even when it
        // ends up empty, or the removed children would load again from it.
        if !meta.is_empty() || (inline && was_inline) {
            dir_syncback.fs_snapshot.add_file(
                &meta_path,
                crate::json::to_vec_pretty(&meta)
                    .context("could not serialize new init.meta.json")?,
            );
            wrote_meta = true;
        }
    }

    let metadata_empty = meta
        .as_ref()
        .map(DirectoryMetadata::is_empty)
        .unwrap_or_default();

    // Children that were stored inline are now written as their own files, so
    // the old entries must not survive to load a second copy of each of them.
    if !inline && was_inline {
        let mut stale = Vec::new();
        if meta_path != jsonc_path {
            stale.push(jsonc_path);
        }
        if metadata_empty {
            stale.push(meta_path);
        }
        for path in stale {
            if is_file(&path)? {
                dir_syncback.fs_snapshot.remove_file(path);
            }
        }
    }

    if new_inst.children().is_empty() && metadata_empty && !wrote_meta {
        dir_syncback
            .fs_snapshot
            .add_file(snapshot.path.join(EMPTY_DIR_KEEP_NAME), Vec::new())
    }

    Ok(dir_syncback)
}

/// Returns whether a directory should store its whole subtree in its
/// `init.meta.json` instead of writing a file for each descendant.
fn should_inline(snapshot: &SyncbackSnapshot) -> bool {
    // Two-way sync rewrites one file at a time, so it keeps whichever layout
    // the directory already has on disk.
    if snapshot.is_scoped() {
        return has_inline_children(snapshot);
    }
    if !instances_as_directories(snapshot.project()) || snapshot.contains_script(snapshot.new) {
        return false;
    }
    if let Some(old_inst) = snapshot.old_inst() {
        // Project-defined children are written by the project file.
        let has_project_children = old_inst.children().iter().any(|child| {
            snapshot.get_old_instance(*child).is_some_and(|child| {
                matches!(
                    child.metadata().instigating_source,
                    Some(InstigatingSource::ProjectNode { .. })
                )
            })
        });
        if has_project_children {
            return false;
        }
    }
    // Ignored subtrees are skipped per path, which an inline descendant lacks.
    let inst_path = snapshot.get_new_inst_path(snapshot.new);
    let prefix = format!("{inst_path}/");
    if snapshot.ignore_tree().is_some_and(|ignored| {
        ignored
            .iter()
            .any(|tree| inst_path.is_empty() || tree.starts_with(&prefix))
    }) {
        return false;
    }
    // A failure to read the rules keeps the per-path layout, which is checked.
    !inline_blocked_by_ignore_paths(snapshot).unwrap_or(true)
}

/// Returns whether `ignorePaths` excludes this directory's metadata or any path
/// its descendants are or would be written to. Inlining moves descendants into
/// the metadata, past the per-path checks syncback applies to each file, so an
/// excluded metadata file would drop the subtree and an excluded child would
/// leave its old file next to an inline copy.
fn inline_blocked_by_ignore_paths(snapshot: &SyncbackSnapshot) -> anyhow::Result<bool> {
    let Some(rules) = &snapshot.project().syncback_rules else {
        return Ok(false);
    };
    let globs = Some(rules.compile_globs()?);
    let base = snapshot.project().folder_location();
    let allowed = |path: &Path| is_valid_path(&globs, base, path);

    if !allowed(&snapshot.path.join("init.meta.json"))
        || !allowed(&snapshot.path.join("init.meta.jsonc"))
    {
        return Ok(true);
    }

    // Paths the descendants are written to today.
    if let Some(old) = snapshot.old {
        let mut pending = vec![old];
        while let Some(referent) = pending.pop() {
            let Some(instance) = snapshot.get_old_instance(referent) else {
                continue;
            };
            pending.extend(instance.children().iter().copied());
            if referent == old {
                continue;
            }
            if let Some(InstigatingSource::Path(path)) = &instance.metadata().instigating_source {
                if !allowed(path) {
                    return Ok(true);
                }
            }
        }
    }

    // Paths the descendants would be written to as their own files.
    let dom = snapshot.new_tree();
    let mut pending = vec![(snapshot.new, snapshot.path.clone())];
    while let Some((referent, directory)) = pending.pop() {
        let parent = dom.get_by_ref(referent).unwrap();
        let stems = unique_child_stems(dom, parent);
        for child_ref in parent.children() {
            let child = dom.get_by_ref(*child_ref).unwrap();
            let middleware = directory_middleware_for_instance(child);
            let stem = &stems[child_ref];
            let (path, meta) = if middleware.is_dir() {
                let path = directory.join(stem);
                let meta = path.join("init.meta.json");
                (path, meta)
            } else {
                let extension = extension_for_middleware(middleware);
                (
                    directory.join(format!("{stem}.{extension}")),
                    directory.join(format!("{stem}.meta.json")),
                )
            };
            if !allowed(&path) || !allowed(&meta) {
                return Ok(true);
            }
            if middleware.is_dir() {
                pending.push((*child_ref, path));
            }
        }
    }
    Ok(false)
}

/// Returns whether the existing directory stores any of its children inline.
fn has_inline_children(snapshot: &SyncbackSnapshot) -> bool {
    snapshot.old_inst().is_some_and(|old_inst| {
        old_inst.children().iter().any(|child| {
            snapshot
                .get_old_instance(*child)
                .is_some_and(|child| child.metadata().instigating_source.is_none())
        })
    })
}

/// Syncs back a directory whose descendants all live in its metadata. Any
/// file-backed children it used to have are removed.
fn syncback_dir_inline<'sync>(snapshot: &SyncbackSnapshot<'sync>) -> SyncbackReturn<'sync> {
    let mut removed_children = Vec::new();
    if !snapshot.is_scoped() {
        if let Some(old_inst) = snapshot.old_inst() {
            for child in old_inst.children() {
                let child = snapshot.get_old_instance(*child).unwrap();
                if matches!(
                    child.metadata().instigating_source,
                    Some(InstigatingSource::Path(_))
                ) {
                    removed_children.push(child);
                }
            }
        }
    }

    SyncbackReturn {
        fs_snapshot: FsSnapshot::new().with_added_dir(&snapshot.path),
        children: Vec::new(),
        removed_children,
    }
}

/// Serializes the children of `new` for its parent's metadata. Each is paired
/// with an existing instance so manually specified IDs survive rewrites.
fn inline_children(snapshot: &SyncbackSnapshot, new: Ref, old: Option<Ref>) -> Vec<InlineInstance> {
    let new_children = snapshot.get_new_instance(new).unwrap().children();
    let old_children: Vec<_> = old
        .and_then(|old| snapshot.get_old_instance(old))
        .map(|old| {
            old.children()
                .iter()
                .filter_map(|child| snapshot.get_old_instance(*child))
                .filter(|child| match child.metadata().instigating_source {
                    None => true,
                    // A scoped rewrite only covers instances already stored inline,
                    // so a file-backed sibling must not lend one of them its ID.
                    Some(InstigatingSource::Path(_)) => !snapshot.is_scoped(),
                    Some(InstigatingSource::ProjectNode { .. }) => false,
                })
                .collect()
        })
        .unwrap_or_default();
    let pairs = pair_children(snapshot, new_children, &old_children);

    new_children
        .iter()
        .zip(pairs)
        .map(|(child, old_child)| {
            let instance = snapshot.get_new_instance(*child).unwrap();
            let (properties, attributes) = serialize_values(
                &instance.class,
                filter_properties(snapshot.project(), instance),
            );
            // An instance's own `Rojo_Id` attribute is saved with its other
            // attributes and is what Ref properties point at, so an ID inherited
            // from the paired old instance only survives when the instance has
            // no attribute and no other instance claims that ID. Reordered
            // duplicate-named siblings can pair with each other's old instances,
            // and writing both IDs would make two instances share one.
            let id = old_child
                .and_then(|old| old.metadata().specified_id.as_ref())
                .map(ToString::to_string)
                .filter(|id| {
                    rojo_id_attribute(instance).is_none() && !snapshot.is_attribute_id(id)
                });

            InlineInstance {
                class_name: instance.class,
                name: Some(instance.name.clone()),
                id,
                properties,
                attributes,
                children: inline_children(snapshot, *child, old_child.map(|old| old.id())),
            }
        })
        .collect()
}

/// Pairs each new child with an old one, preferring the same position, then the
/// same name and class, then the same position and class to follow renames.
fn pair_children<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
    new_children: &[Ref],
    old_children: &[InstanceWithMeta<'sync>],
) -> Vec<Option<InstanceWithMeta<'sync>>> {
    let new: Vec<_> = new_children
        .iter()
        .map(|child| {
            let child = snapshot.get_new_instance(*child).unwrap();
            (child.name.as_str(), child.class)
        })
        .collect();
    let old: Vec<_> = old_children
        .iter()
        .map(|child| (child.name(), child.class_name()))
        .collect();
    pair_indices(&new, &old)
        .into_iter()
        .map(|pair| pair.map(|old_index| old_children[old_index]))
        .collect()
}

/// Pairs children by name and class, returning the old index for each new one.
/// Every pass is linear so rewriting a large inline folder stays cheap.
fn pair_indices(new: &[(&str, Ustr)], old: &[(&str, Ustr)]) -> Vec<Option<usize>> {
    let mut taken = vec![false; old.len()];
    let mut pairs: Vec<Option<usize>> = vec![None; new.len()];
    let shared = new.len().min(old.len());

    for index in 0..shared {
        if new[index] == old[index] {
            taken[index] = true;
            pairs[index] = Some(index);
        }
    }

    // Queues keep duplicates in child order, so the nth `Model` pairs with the
    // nth remaining old `Model`.
    let mut queues: HashMap<(&str, Ustr), VecDeque<usize>> = HashMap::new();
    for (old_index, key) in old.iter().enumerate() {
        if !taken[old_index] {
            queues.entry(*key).or_default().push_back(old_index);
        }
    }
    for (index, pair) in pairs.iter_mut().enumerate() {
        if pair.is_some() {
            continue;
        }
        if let Some(old_index) = queues.get_mut(&new[index]).and_then(VecDeque::pop_front) {
            taken[old_index] = true;
            *pair = Some(old_index);
        }
    }

    // A renamed child keeps its position and class.
    for index in 0..shared {
        if pairs[index].is_none() && !taken[index] && new[index].1 == old[index].1 {
            taken[index] = true;
            pairs[index] = Some(index);
        }
    }

    pairs
}

fn rojo_id_attribute(instance: &rbx_dom_weak::Instance) -> Option<&str> {
    match instance.properties.get(&ustr("Attributes")) {
        Some(Variant::Attributes(attributes)) => match attributes.get(REF_ID_ATTRIBUTE_NAME) {
            Some(Variant::String(id)) => Some(id),
            _ => None,
        },
        _ => None,
    }
}

pub fn syncback_dir_no_meta<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();

    let mut children = Vec::new();
    let mut removed_children = Vec::new();

    if instances_as_directories(snapshot.project()) {
        // Duplicate and unsafe names are given unique file names instead of
        // being rejected, so children are matched to the old tree by the file
        // name they would be written to rather than by their Instance name.
        let stems = unique_child_stems(snapshot.new_tree(), new_inst);
        let mut old_child_map = HashMap::new();
        let mut project_children = HashMap::new();
        if let Some(old_inst) = snapshot.old_inst() {
            for child in old_inst.children() {
                let inst = snapshot.get_old_instance(*child).unwrap();
                if matches!(
                    inst.metadata().instigating_source,
                    Some(InstigatingSource::ProjectNode { .. })
                ) {
                    // The project syncback handles one child per matching
                    // project node, so only skip that many new children.
                    *project_children.entry(inst.name()).or_insert(0) += 1;
                    continue;
                }
                let file_name = inst
                    .metadata()
                    .relevant_paths
                    .first()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str());
                if let Some(file_name) = file_name {
                    old_child_map.insert(file_name.to_lowercase(), inst);
                }
            }
        }

        for new_child_ref in new_inst.children() {
            let new_child = snapshot.get_new_instance(*new_child_ref).unwrap();
            if let Some(remaining) = project_children.get_mut(new_child.name.as_str()) {
                *remaining -= 1;
                if *remaining == 0 {
                    project_children.remove(new_child.name.as_str());
                }
                continue;
            }
            let stem = &stems[new_child_ref];
            let fresh = snapshot.with_stem(&snapshot.path, *new_child_ref, None, stem);
            let file_name = fresh
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_lowercase);
            let old_child: Option<InstanceWithMeta> =
                file_name.and_then(|name| old_child_map.remove(&name));
            children.push(match old_child {
                Some(old_child) => {
                    snapshot.with_stem(&snapshot.path, *new_child_ref, Some(old_child.id()), stem)
                }
                None => fresh,
            });
        }
        removed_children.extend(old_child_map.into_values());
    } else {
        children_by_name(snapshot, &mut children, &mut removed_children)?;
    }

    let mut fs_snapshot = FsSnapshot::new();

    if let Some(old_ref) = snapshot.old {
        let new_hash = hash_instance(snapshot.project(), snapshot.new_tree(), snapshot.new)
            .expect("new Instance should be hashable");
        let old_hash = hash_instance(snapshot.project(), snapshot.old_tree(), old_ref)
            .expect("old Instance should be hashable");

        if old_hash != new_hash {
            fs_snapshot.add_dir(&snapshot.path);
        } else {
            log::debug!(
                "Skipping reserializing directory {} because old and new tree hash the same",
                new_inst.name
            );
        }
    } else {
        fs_snapshot.add_dir(&snapshot.path);
    }

    Ok(SyncbackReturn {
        fs_snapshot,
        children,
        removed_children,
    })
}

/// Pairs the children of a directory with the old tree by Instance name, which
/// requires every child to have a unique name.
fn children_by_name<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
    children: &mut Vec<SyncbackSnapshot<'sync>>,
    removed_children: &mut Vec<InstanceWithMeta<'sync>>,
) -> anyhow::Result<()> {
    let new_inst = snapshot.new_inst();

    // We have to enforce unique child names for the file system.
    let mut child_names = HashSet::with_capacity(new_inst.children().len());
    let mut duplicate_set = HashSet::new();
    for child_ref in new_inst.children() {
        let child = snapshot.get_new_instance(*child_ref).unwrap();
        if !child_names.insert(child.name.to_lowercase()) {
            duplicate_set.insert(child.name.as_str());
        }
    }
    if !duplicate_set.is_empty() {
        if duplicate_set.len() <= 25 {
            anyhow::bail!(
                "Instance has children with duplicate name (case may not exactly match):\n {}",
                duplicate_set.into_iter().collect::<Vec<&str>>().join(", ")
            );
        }
        anyhow::bail!("Instance has more than 25 children with duplicate names");
    }

    if let Some(old_inst) = snapshot.old_inst() {
        let mut old_child_map = HashMap::with_capacity(old_inst.children().len());
        for child in old_inst.children() {
            let inst = snapshot.get_old_instance(*child).unwrap();
            old_child_map.insert(inst.name(), inst);
        }

        for new_child_ref in new_inst.children() {
            let new_child = snapshot.get_new_instance(*new_child_ref).unwrap();
            if let Some(old_child) = old_child_map.remove(new_child.name.as_str()) {
                if old_child.metadata().relevant_paths.is_empty() {
                    log::debug!(
                        "Skipping instance {} because it doesn't exist on the disk",
                        old_child.name()
                    );
                    continue;
                } else if matches!(
                    old_child.metadata().instigating_source,
                    Some(InstigatingSource::ProjectNode { .. })
                ) {
                    log::debug!(
                        "Skipping instance {} because it originates in a project file",
                        old_child.name()
                    );
                    continue;
                }
                // This child exists in both doms. Pass it on.
                children.push(snapshot.with_joined_path(*new_child_ref, Some(old_child.id()))?);
            } else {
                // The child only exists in the the new dom
                children.push(snapshot.with_joined_path(*new_child_ref, None)?);
            }
        }
        // Any children that are in the old dom but not the new one are removed.
        removed_children.extend(old_child_map.into_values());
    } else {
        // There is no old instance. Just add every child.
        for new_child_ref in new_inst.children() {
            children.push(snapshot.with_joined_path(*new_child_ref, None)?);
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    fn keys(items: &[(&'static str, &'static str)]) -> Vec<(&'static str, Ustr)> {
        items
            .iter()
            .map(|(name, class)| (*name, ustr(class)))
            .collect()
    }

    #[test]
    fn pairing_prefers_position_then_name_then_renames() {
        let old = keys(&[
            ("A", "Part"),
            ("B", "Part"),
            ("C", "Folder"),
            ("Old", "Part"),
        ]);
        let new = keys(&[
            ("B", "Part"),
            ("A", "Part"),
            ("C", "Folder"),
            ("New", "Part"),
        ]);
        // C keeps its slot, A and B find each other by name, and Old is renamed.
        assert_eq!(
            pair_indices(&new, &old),
            [Some(1), Some(0), Some(2), Some(3)]
        );
        // A class change is never treated as a rename.
        let old = keys(&[("X", "Part")]);
        let new = keys(&[("Y", "Folder")]);
        assert_eq!(pair_indices(&new, &old), [None]);
    }

    #[test]
    fn pairing_keeps_duplicates_in_order() {
        let old = keys(&[
            ("A", "Part"),
            ("B", "Part"),
            ("Model", "Model"),
            ("Model", "Model"),
        ]);
        let new = keys(&[("Model", "Model"), ("Model", "Model")]);
        // Neither Model kept its slot, so each takes the next remaining one in order.
        assert_eq!(pair_indices(&new, &old), [Some(2), Some(3)]);
    }

    /// Large folders of identical children are common in maps, so pairing must
    /// stay linear rather than rescanning the old children for each new one.
    #[test]
    fn pairing_large_duplicate_groups_stays_linear() {
        let old: Vec<_> = (0..200_000).map(|_| ("Part", ustr("Part"))).collect();
        let mut new = vec![("Moved", ustr("Folder"))];
        new.extend(old.iter().copied());
        let pairs = pair_indices(&new, &old);
        // Slots that still line up keep their pair, and the one Part pushed past
        // the end takes the only old Part left.
        assert_eq!(pairs[0], None);
        assert!((1..old.len()).all(|index| pairs[index] == Some(index)));
        assert_eq!(pairs[old.len()], Some(0));
    }

    /// Two-way sync removing the last inline child must still rewrite the
    /// metadata, as a single-file write, without staging a `.gitkeep`.
    #[test]
    fn removing_the_last_inline_child_rewrites_the_metadata() {
        use crate::{
            project::Project,
            snapshot::RojoTree,
            syncback::{syncback_instance, FsSnapshot},
        };
        use std::path::PathBuf;

        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{"name":"Test","tree":{"$className":"Folder"},"syncbackRules":{"instancesAsDirectories":true}}"#,
                    ),
                ),
                (
                    "Holder",
                    VfsSnapshot::dir([(
                        "init.meta.json",
                        VfsSnapshot::file(
                            r#"{"children":[{"className":"Folder","name":"Child"}]}"#,
                        ),
                    )]),
                ),
            ]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let holder = snapshot_from_vfs(&InstanceContext::new(), &vfs, Path::new("/project/Holder"))
            .unwrap()
            .unwrap();
        assert_eq!(holder.children.len(), 1);
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![holder]));
        let holder_id = tree.root().children()[0];
        let expected = FsSnapshot::capture_file_contents(
            &tree.get_metadata(holder_id).unwrap().relevant_paths,
            &vfs,
        )
        .unwrap();

        let new_tree = rbx_dom_weak::WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("Holder"),
        );
        let staged = syncback_instance(&vfs, &tree, &project, holder_id, new_tree).unwrap();
        staged
            .write_single_file_to_vfs(project.folder_location(), &vfs, &expected)
            .unwrap();

        assert!(vfs
            .metadata("/project/Holder/.gitkeep")
            .with_not_found()
            .unwrap()
            .is_none());
        let reloaded =
            snapshot_from_vfs(&InstanceContext::new(), &vfs, Path::new("/project/Holder"))
                .unwrap()
                .unwrap();
        assert!(reloaded.children.is_empty());
    }

    #[test]
    fn parallel_children_preserve_nested_order_and_sources() {
        let mut imfs = InMemoryFs::new();
        let groups = (0..16).map(|group| {
            let scripts = (0..16).map(|script| {
                (
                    format!("Child{script:02}.luau"),
                    VfsSnapshot::file(format!("return {group} * 16 + {script}")),
                )
            });
            (format!("Group{group:02}"), VfsSnapshot::dir(scripts))
        });
        imfs.load_snapshot("/root", VfsSnapshot::dir(groups))
            .unwrap();
        let vfs = Vfs::new(imfs);
        vfs.set_watch_enabled(false);
        let load = |workers| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap()
                .install(|| {
                    snapshot_from_vfs(&InstanceContext::default(), &vfs, Path::new("/root"))
                        .unwrap()
                        .unwrap()
                })
        };
        let serial = load(1);
        for workers in [2, 4] {
            let parallel = load(workers);
            assert_eq!(
                serde_json::to_value(&serial).unwrap(),
                serde_json::to_value(&parallel).unwrap()
            );
            for (group, child) in parallel.children.iter().enumerate() {
                assert_eq!(child.name, format!("Group{group:02}"));
                for (script, child) in child.children.iter().enumerate() {
                    assert_eq!(child.name, format!("Child{script:02}"));
                    assert_eq!(
                        child.properties[&rbx_dom_weak::ustr("Source")],
                        rbx_dom_weak::types::Variant::String(format!(
                            "return {group} * 16 + {script}"
                        ))
                    );
                }
            }
        }
    }

    #[test]
    fn parallel_children_report_first_ordered_error() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir((0..16).map(|index| {
                (
                    format!("Child{index:02}.model.json"),
                    VfsSnapshot::file("not json"),
                )
            })),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);
        for workers in [1, 2, 4] {
            let error = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap()
                .install(|| {
                    snapshot_from_vfs(&InstanceContext::default(), &vfs, Path::new("/root"))
                })
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("Child00.model.json"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn empty_folder() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo", VfsSnapshot::empty_dir())
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_dir(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo"),
            "foo",
            None,
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn folder_in_folder() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir([("Child", VfsSnapshot::empty_dir())]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_dir(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo"),
            "foo",
            None,
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }
}
