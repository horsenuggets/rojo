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
        free_child_stem, hash_json_instance, instances_as_directories, is_valid_path,
        sibling_number, sibling_order_key, unique_child_stems, FsSnapshot, SyncbackReturn,
        SyncbackSnapshot,
    },
    REF_ID_ATTRIBUTE_NAME,
};

use super::{
    meta_file::{serialize_values, DirectoryMetadata, InlineInstance},
    snapshot_from_vfs, Middleware,
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
    // numbers `.__2`, `.__3`, ... and a plain listing puts `.__10` before `.__2`.
    // Legacy `~N` exports retain the same numeric ordering.
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
    let numbered_children = entries
        .iter()
        .zip(children)
        .filter_map(|(entry, child)| {
            let file_name = entry.path().file_name()?.to_string_lossy();
            let child = child?;
            let number = sibling_number(&file_name, &child.name, &child.class_name);
            Some((number, child))
        })
        .collect();
    let snapshot_children = order_duplicate_siblings(numbered_children);

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

/// Puts children that share a name back in the order syncback numbered them, given
/// each child with the `.__N` or legacy `~N` number from its file name (1 when absent).
///
/// Sorting directory entries by their sibling order key keeps `Twin.__2.luau`
/// after `Twin.luau`, but not when siblings are written differently. `Twin.__2/`
/// has a key before `Twin.luau`, which would reverse them. Only same-named children
/// move, and each group keeps its positions. Syncback numbers names regardless of
/// case, so they're grouped the same way.
fn order_duplicate_siblings(children: Vec<(u64, InstanceSnapshot)>) -> Vec<InstanceSnapshot> {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (_, child)) in children.iter().enumerate() {
        groups
            .entry(child.name.to_lowercase())
            .or_default()
            .push(index);
    }

    let mut slots: Vec<_> = children.into_iter().map(Some).collect();
    for positions in groups.values().filter(|positions| positions.len() > 1) {
        let mut members: Vec<_> = positions
            .iter()
            .map(|index| slots[*index].take().unwrap())
            .collect();
        members.sort_by_key(|(number, _)| *number);
        for (index, member) in positions.iter().zip(members) {
            slots[*index] = Some(member);
        }
    }
    slots.into_iter().map(|slot| slot.unwrap().1).collect()
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
        // ends up empty, or the removed children would load again from it. So
        // is any metadata file that already exists, or the values it held, like
        // the last attribute removed from a folder, would load again. Emptying
        // it in place also keeps the edit to one file, which two-way sync needs.
        // A file whose inline children are moving out is removed below instead.
        let leaving_inline = !inline && was_inline;
        if !meta.is_empty() || (inline && was_inline) || (!leaving_inline && is_file(&meta_path)?) {
            let contents = crate::json::to_vec_pretty(&meta)
                .context("could not serialize new init.meta.json")?;
            // A fresh relayout removes the old directory before writing, including
            // metadata whose serialized bytes happen to be unchanged.
            if snapshot.metadata_source.is_some()
                || snapshot.vfs().read(&meta_path).with_not_found()?.as_deref() != Some(&contents)
            {
                dir_syncback.fs_snapshot.add_file(&meta_path, contents);
            }
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
    // A model file needs a path of its own, which an inline descendant lacks.
    if snapshot.contains_model_tree(snapshot.new) {
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
        // A directory with a model tree is never inlined, so none of these
        // descendants is written as a model file.
        let stems = unique_child_stems(dom, parent, None);
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

    pair_remaining_by_name(new, old, &mut pairs, &mut taken);

    // A renamed child keeps its position and class.
    for index in 0..shared {
        if pairs[index].is_none() && !taken[index] && new[index].1 == old[index].1 {
            taken[index] = true;
            pairs[index] = Some(index);
        }
    }

    pairs
}

/// Queues keep duplicates in sibling order, without reusing a consumed entry.
fn pair_remaining_by_name(
    new: &[(&str, Ustr)],
    old: &[(&str, Ustr)],
    pairs: &mut [Option<usize>],
    taken: &mut [bool],
) {
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
}

/// Pair logical siblings in their original order, independently of generated
/// stems. A released or sparse suffix must not steal another sibling's identity
/// just because it matches a newly allocated physical path.
fn pair_directory_indices(new: &[(&str, Ustr)], old: &[(&str, Ustr)]) -> Vec<Option<usize>> {
    let mut taken = vec![false; old.len()];
    let mut pairs = vec![None; new.len()];
    pair_remaining_by_name(new, old, &mut pairs, &mut taken);
    pairs
}

/// A class change or same-class rename can reuse only the unconsumed source at
/// its exact normalized output path. Logical name-and-class pairs have priority,
/// including retained legacy paths.
fn pair_directory_replacements(
    new: &[(&str, Ustr)],
    old: &[(&str, Ustr)],
    outputs: &[String],
    old_paths: &[String],
    pairs: &mut [Option<usize>],
) {
    let taken: HashSet<_> = pairs.iter().flatten().copied().collect();
    let claimed: HashSet<_> = outputs
        .iter()
        .zip(pairs.iter())
        .filter_map(|(path, pair)| pair.is_some().then_some(path))
        .collect();
    let mut available = HashMap::new();
    for (index, path) in old_paths.iter().enumerate() {
        // Ambiguous physical ownership must not transfer arbitrary metadata,
        // even if another owner has already paired and is moving elsewhere.
        available
            .entry(path)
            .and_modify(|entry| *entry = None)
            .or_insert_with(|| (!taken.contains(&index)).then_some(index));
    }
    for (index, pair) in pairs.iter_mut().enumerate() {
        if pair.is_some() || claimed.contains(&outputs[index]) {
            continue;
        }
        if let Some(Some(old_index)) = available.remove(&outputs[index]) {
            if new[index].0 == old[old_index].0 || new[index].1 == old[old_index].1 {
                *pair = Some(old_index);
            }
        }
    }
}

/// Canonical script suffix migrations retain their existing physical allocation
/// policy. Falling back by duplicate name here would send two sources to the same
/// canonical destination; unchanged (including released) paths use ordered pairing.
fn pair_script_migration_indices(
    new: &[(&str, Ustr)],
    old: &[(&str, Ustr)],
    new_paths: &[String],
    old_paths: &[String],
    migrating: &HashSet<(&str, Ustr)>,
    pairs: &mut [Option<usize>],
) {
    let mut old_groups: HashMap<_, Vec<usize>> = HashMap::new();
    let mut new_counts = HashMap::new();
    let mut paths = HashMap::new();
    for (index, key) in old.iter().enumerate() {
        old_groups.entry(*key).or_default().push(index);
        paths.insert((old_paths[index].as_str(), *key), index);
    }
    for key in new {
        *new_counts.entry(*key).or_insert(0) += 1;
    }
    for (index, key) in new.iter().enumerate() {
        if !migrating.contains(key) {
            continue;
        }
        pairs[index] = paths
            .remove(&(new_paths[index].as_str(), *key))
            .or_else(|| {
                let group = old_groups.get(key)?;
                (group.len() == 1 && new_counts[key] == 1).then_some(group[0])
            });
    }
}

/// A model relayout can displace a retained source, which must then use its own
/// allocated path. Other collisions still go through the existing write guards.
fn directory_fresh_outputs(
    fresh: &[String],
    kept: &[Option<String>],
    relayout: &[bool],
) -> Vec<bool> {
    let mut paths: HashMap<_, _> = kept
        .iter()
        .enumerate()
        .filter_map(|(index, path)| path.as_ref().map(|path| (path, index)))
        .collect();
    let mut needs_fresh: Vec<_> = kept.iter().map(Option::is_none).collect();
    let mut pending: Vec<_> = relayout
        .iter()
        .enumerate()
        .filter_map(|(index, relayout)| relayout.then_some(index))
        .collect();
    while let Some(index) = pending.pop() {
        if let Some(displaced) = paths.remove(&fresh[index]) {
            if !needs_fresh[displaced] {
                needs_fresh[displaced] = true;
                pending.push(displaced);
            }
        }
    }
    needs_fresh
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

/// Class replacements cannot retain a class-defining initializer, or keep a file
/// where the new class needs a directory. Unchanged classes retain the existing
/// canonical-script and model-relayout policies.
fn keeps_directory_layout(snapshot: &SyncbackSnapshot, new: Ref, old: InstanceWithMeta) -> bool {
    if !snapshot.keeps_model_layout(new, old) {
        return false;
    }
    if snapshot.get_new_instance(new).unwrap().class == old.class_name() {
        return true;
    }
    let Some(old_middleware) = old.metadata().middleware else {
        return true;
    };
    // A changed initializer must follow the incoming class, not inherit the
    // removed class's middleware in the default (non-directory) layout mode.
    let fresh = snapshot.with_new_path(snapshot.path.clone(), new, None);
    let middleware = crate::syncback::get_best_middleware(&fresh);
    let changes_initializer = matches!(
        old.class_name().as_str(),
        "Script" | "LocalScript" | "ModuleScript" | "LocalizationTable"
    ) || snapshot.get_new_instance(new).unwrap().class
        == "LocalizationTable";
    old_middleware.is_dir() == middleware.is_dir()
        && (!changes_initializer || old_middleware == middleware)
}

/// Full syncback must append after actual retained ordinals, not the count of
/// incoming siblings. The shared allocator also checks pending sidecar owners.
fn appended_directory_stem(
    child: &rbx_dom_weak::Instance,
    middleware: Middleware,
    maximum: u64,
    reserved: &HashSet<String>,
) -> anyhow::Result<String> {
    let required = maximum.checked_add(1).ok_or_else(|| {
        crate::syncback::OwnershipConflict(format!(
            "Sibling ordinals are exhausted for instance {:?}",
            child.name
        ))
    })?;
    free_child_stem(child, middleware, reserved, required)
        .map_err(|error| crate::syncback::OwnershipConflict(error.to_string()).into())
}

fn directory_output_names(child: &SyncbackSnapshot) -> Vec<String> {
    let mut names = vec![child
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase()];
    if !crate::syncback::get_best_middleware(child).is_dir() {
        let stem = child.file_stem();
        names.push(format!("{stem}.meta.json").to_lowercase());
        names.push(format!("{stem}.meta.jsonc").to_lowercase());
    }
    names
}

fn reserve_directory_output(child: &SyncbackSnapshot, reserved: &mut HashSet<String>) {
    reserved.extend(directory_output_names(child));
}

/// Replacements must keep their logical ordinal exactly. Advancing past a taken
/// path could put this sibling after another retained sibling on the next reload.
fn replacement_directory_stem(
    child: &rbx_dom_weak::Instance,
    middleware: Middleware,
    number: u64,
    reserved: &HashSet<String>,
) -> anyhow::Result<String> {
    let stem = free_child_stem(child, middleware, reserved, number)
        .map_err(|error| crate::syncback::OwnershipConflict(error.to_string()))?;
    if sibling_number(&stem, &child.name, &child.class) != number {
        return Err(crate::syncback::OwnershipConflict(format!(
            "Cannot preserve sibling ordinal {number} for instance {:?} because another source owns its destination",
            child.name
        )).into());
    }
    Ok(stem)
}

pub fn syncback_dir_no_meta<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();

    let mut children = Vec::new();
    let mut removed_children = Vec::new();

    if instances_as_directories(snapshot.project()) {
        let mut stems = unique_child_stems(snapshot.new_tree(), new_inst, snapshot.model_trees());
        let mut old_children = Vec::new();
        let mut old_paths = Vec::new();
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
                    old_paths.push(file_name.to_lowercase());
                    old_children.push(inst);
                }
            }
        }

        let mut fresh_children = Vec::new();
        let mut new_keys = Vec::new();
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
            new_keys.push((new_child.name.as_str(), new_child.class));
            fresh_children.push(fresh);
        }
        let old_keys: Vec<_> = old_children
            .iter()
            .map(|child| (child.name(), child.class_name()))
            .collect();
        let mut pairs = pair_directory_indices(&new_keys, &old_keys);
        let physical_name = |child: &SyncbackSnapshot<'_>| {
            child
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
        };
        let mut fresh_paths: Vec<_> = fresh_children.iter().map(physical_name).collect();
        let migrating: HashSet<_> = fresh_children
            .iter()
            .zip(&pairs)
            .enumerate()
            .filter_map(|(index, (fresh, pair))| {
                let old_index = (*pair)?;
                let old = old_children[old_index];
                if !keeps_directory_layout(snapshot, fresh.new, old) {
                    return None;
                }
                let paired = snapshot.with_stem(
                    &snapshot.path,
                    fresh.new,
                    Some(old.id()),
                    &stems[&fresh.new],
                );
                (physical_name(&paired) != old_paths[old_index]).then_some(new_keys[index])
            })
            .collect();
        if !migrating.is_empty() {
            pair_script_migration_indices(
                &new_keys,
                &old_keys,
                &fresh_paths,
                &old_paths,
                &migrating,
                &mut pairs,
            );
        }
        let paired_outputs: Vec<_> = fresh_children
            .iter()
            .zip(&pairs)
            .map(|(fresh, pair)| {
                pair.filter(|index| {
                    keeps_directory_layout(snapshot, fresh.new, old_children[*index])
                })
                .map(|index| {
                    physical_name(&snapshot.with_stem(
                        &snapshot.path,
                        fresh.new,
                        Some(old_children[index].id()),
                        &stems[&fresh.new],
                    ))
                })
                .unwrap_or_else(|| physical_name(fresh))
            })
            .collect();
        pair_directory_replacements(
            &new_keys,
            &old_keys,
            &paired_outputs,
            &old_paths,
            &mut pairs,
        );
        let relayout: Vec<_> = fresh_children
            .iter()
            .zip(&pairs)
            .map(|(fresh, pair)| {
                pair.is_some_and(|index| {
                    !keeps_directory_layout(snapshot, fresh.new, old_children[index])
                })
            })
            .collect();
        // Assign potential replacements before deciding which retained sources a
        // relayout displaces. Vector positions backfill sparse exports and can
        // reverse mixed-layout siblings, including legacy and natural names.
        let mut seen_names = HashSet::new();
        let duplicate_names: HashSet<_> = new_keys
            .iter()
            .map(|key| key.0.to_lowercase())
            .filter(|name| !seen_names.insert(name.clone()))
            .collect();
        let mut preserved_numbers = vec![None; fresh_children.len()];
        for (index, fresh) in fresh_children.iter_mut().enumerate() {
            let Some(old_index) = pairs[index] else {
                continue;
            };
            let old = old_children[old_index];
            if migrating.contains(&new_keys[index])
                || old.name().to_lowercase() != new_keys[index].0.to_lowercase()
            {
                continue;
            }
            let number = sibling_number(&old_paths[old_index], old.name(), &old.class_name());
            // A retained singleton may already have a higher allocation to make
            // way for an unrelated model (FX versus FX.rbxm). That displacement
            // cannot reorder logical peers, but must never backfill a sparse floor.
            let allocated =
                sibling_number(&fresh_paths[index], new_keys[index].0, &new_keys[index].1);
            if !relayout[index]
                && !duplicate_names.contains(&new_keys[index].0.to_lowercase())
                && allocated > number
            {
                preserved_numbers[index] = Some(allocated);
                continue;
            }
            let stem = replacement_directory_stem(
                fresh.new_inst(),
                crate::syncback::get_best_middleware(fresh),
                number,
                &HashSet::new(),
            )?;
            *fresh = snapshot.with_stem(&snapshot.path, fresh.new, None, &stem);
            fresh_paths[index] = physical_name(fresh);
            stems.insert(fresh.new, stem);
            preserved_numbers[index] = Some(number);
        }
        let kept_paths: Vec<_> = fresh_children
            .iter()
            .zip(&pairs)
            .map(|(fresh, pair)| {
                let old = old_children[(*pair)?];
                keeps_directory_layout(snapshot, fresh.new, old).then(|| {
                    physical_name(&snapshot.with_stem(
                        &snapshot.path,
                        fresh.new,
                        Some(old.id()),
                        &stems[&fresh.new],
                    ))
                })
            })
            .collect();
        let needs_fresh = directory_fresh_outputs(&fresh_paths, &kept_paths, &relayout);
        let consumed: HashSet<_> = pairs.iter().flatten().copied().collect();
        let retained_names: HashSet<_> = pairs
            .iter()
            .enumerate()
            .filter_map(|(index, pair)| pair.map(|_| new_keys[index].0.to_lowercase()))
            .collect();
        let unconsumed_names: HashSet<_> = old_keys
            .iter()
            .enumerate()
            .filter(|(index, _)| !consumed.contains(index))
            .map(|(_, key)| key.0.to_lowercase())
            .collect();
        // A dropped migration pair or an unconsumed same-named owner still goes
        // through the existing conflict guards. Only genuine additions append.
        let additions: Vec<_> = new_keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                pairs[index].is_none()
                    && !migrating.contains(key)
                    && retained_names.contains(&key.0.to_lowercase())
                    && !unconsumed_names.contains(&key.0.to_lowercase())
            })
            .collect();
        if additions.iter().any(|addition| *addition) {
            let mut maximums: HashMap<String, u64> = HashMap::new();
            let mut reserved = HashSet::new();
            if snapshot
                .vfs()
                .metadata(&snapshot.path)
                .with_not_found()?
                .is_some_and(|metadata| metadata.is_dir())
            {
                for entry in snapshot.vfs().read_dir(&snapshot.path)? {
                    let entry = entry?;
                    if let Some(name) = entry.path().file_name() {
                        reserved.insert(name.to_string_lossy().to_lowercase());
                    }
                }
            }
            for (index, fresh) in fresh_children.iter().enumerate() {
                if additions[index] {
                    continue;
                }
                let output = if needs_fresh[index] {
                    snapshot.with_new_path(fresh.path.clone(), fresh.new, None)
                } else {
                    snapshot.with_stem(
                        &snapshot.path,
                        fresh.new,
                        pairs[index].map(|old| old_children[old].id()),
                        &stems[&fresh.new],
                    )
                };
                reserve_directory_output(&output, &mut reserved);
                let ordinal = sibling_number(
                    &physical_name(&output),
                    new_keys[index].0,
                    &new_keys[index].1,
                );
                maximums
                    .entry(new_keys[index].0.to_lowercase())
                    .and_modify(|maximum| *maximum = (*maximum).max(ordinal))
                    .or_insert(ordinal);
            }
            for (index, fresh) in fresh_children.iter_mut().enumerate() {
                if !additions[index] {
                    continue;
                }
                let child = fresh.new_inst();
                let key = child.name.to_lowercase();
                let middleware = crate::syncback::get_best_middleware(fresh);
                let stem = appended_directory_stem(child, middleware, maximums[&key], &reserved)?;
                *fresh = snapshot.with_stem(&snapshot.path, fresh.new, None, &stem);
                fresh_paths[index] = physical_name(fresh);
                maximums.insert(
                    key,
                    sibling_number(&fresh_paths[index], &child.name, &child.class),
                );
                reserve_directory_output(fresh, &mut reserved);
                stems.insert(fresh.new, stem);
            }
        }
        let mut outputs = HashSet::new();
        let unpaired_paths: HashSet<_> = old_paths
            .iter()
            .enumerate()
            .filter_map(|(index, path)| (!consumed.contains(&index)).then_some(path))
            .collect();
        for (index, fresh) in fresh_paths.iter().enumerate() {
            let output = if needs_fresh[index] {
                fresh
            } else {
                kept_paths[index].as_ref().unwrap()
            };
            if !outputs.insert(output) || unpaired_paths.contains(output) {
                return Err(crate::syncback::OwnershipConflict(format!(
                    "Cannot stage directory children at the same destination {} because another source owns its destination",
                    snapshot.path.join(output).display()
                )).into());
            }
        }
        if preserved_numbers
            .iter()
            .zip(&needs_fresh)
            .any(|(number, fresh)| number.is_some() && *fresh)
        {
            // Only paired, freshly rebuilt owners release their source paths.
            // Unpaired removals, unknown files, and project-owned entries cannot
            // justify reusing a destination, even if they would later disappear.
            let removed: HashSet<_> = pairs
                .iter()
                .zip(&needs_fresh)
                .filter_map(|(pair, fresh)| pair.filter(|_| *fresh))
                .collect();
            // Ownership uses actual physical paths, not collision keys. On a
            // case-sensitive filesystem, TWIN.meta.json cannot release an
            // unrelated Twin.meta.json entry. Canonicalize tracked paths to use
            // their on-disk spelling even when the filesystem ignores case.
            let mut source_owners: HashMap<_, Vec<usize>> = HashMap::new();
            for (index, old) in old_children.iter().enumerate() {
                if let Some(source) = snapshot
                    .vfs()
                    .canonicalize(&old.metadata().relevant_paths[0])
                    .with_not_found()?
                {
                    source_owners.entry(source).or_default().push(index);
                }
                if old
                    .metadata()
                    .middleware
                    .is_some_and(|middleware| !middleware.is_dir())
                {
                    let source = &old.metadata().relevant_paths[0];
                    for path in old.metadata().relevant_paths.iter().skip(1) {
                        if path.parent() == source.parent()
                            && path.file_name().is_some_and(|name| {
                                let name = name.to_string_lossy();
                                name.ends_with(".meta.json") || name.ends_with(".meta.jsonc")
                            })
                        {
                            if let Some(path) =
                                snapshot.vfs().canonicalize(path).with_not_found()?
                            {
                                source_owners.entry(path).or_default().push(index);
                            }
                        }
                    }
                }
            }
            let mut disk_reserved = HashSet::new();
            if snapshot
                .vfs()
                .metadata(&snapshot.path)
                .with_not_found()?
                .is_some_and(|meta| meta.is_dir())
            {
                let directory = snapshot.vfs().canonicalize(&snapshot.path)?;
                for entry in snapshot.vfs().read_dir(&snapshot.path)? {
                    let entry = entry?;
                    let name = entry.path().file_name().unwrap();
                    // Keep this entry's exact name rather than resolving it as
                    // a symlink, which could lend another entry's ownership.
                    let physical = directory.join(name);
                    if !source_owners
                        .get(&physical)
                        .is_some_and(|owners| owners.iter().all(|owner| removed.contains(owner)))
                    {
                        disk_reserved.insert(name.to_string_lossy().to_lowercase());
                    }
                }
            }
            let pending: Vec<_> = fresh_children
                .iter()
                .enumerate()
                .map(|(index, fresh)| {
                    if needs_fresh[index] {
                        directory_output_names(fresh)
                    } else {
                        directory_output_names(&snapshot.with_stem(
                            &snapshot.path,
                            fresh.new,
                            pairs[index].map(|old| old_children[old].id()),
                            &stems[&fresh.new],
                        ))
                    }
                })
                .collect();
            let mut counts = HashMap::new();
            for path in pending.iter().flatten() {
                *counts.entry(path.clone()).or_insert(0usize) += 1;
            }
            let mut reserved = disk_reserved.clone();
            reserved.extend(counts.keys().cloned());
            for (index, fresh) in fresh_children.iter().enumerate() {
                let Some(number) = preserved_numbers[index].filter(|_| needs_fresh[index]) else {
                    continue;
                };
                // Remove only this output's own reservation. Counts keep a
                // colliding peer's file or either sidecar reserved in linear time.
                for path in &pending[index] {
                    if counts[path] == 1 && !disk_reserved.contains(path) {
                        reserved.remove(path);
                    }
                }
                replacement_directory_stem(
                    fresh.new_inst(),
                    crate::syncback::get_best_middleware(fresh),
                    number,
                    &reserved,
                )?;
                reserved.extend(pending[index].iter().cloned());
            }
        }
        let mut old_children: Vec<_> = old_children.into_iter().map(Some).collect();
        let mut layout_changed = false;
        for ((fresh, pair), needs_fresh) in fresh_children.into_iter().zip(pairs).zip(needs_fresh) {
            let old_child = pair.and_then(|index| old_children[index].take());
            if let Some(old_child) = old_child.filter(|_| needs_fresh) {
                snapshot.validate_layout_change(old_child)?;
                removed_children.push(old_child);
                layout_changed = true;
                children.push(snapshot.with_fresh_path(fresh.path, fresh.new, old_child.id()));
                continue;
            }
            children.push(match old_child {
                Some(old_child) => snapshot.with_stem(
                    &snapshot.path,
                    fresh.new,
                    Some(old_child.id()),
                    &stems[&fresh.new],
                ),
                None => fresh,
            });
        }
        // Relayout outputs can also replace unpaired old sources. Check those
        // removals for ignored files just like the explicitly displaced sources.
        if layout_changed {
            for old_child in old_children.iter().flatten() {
                snapshot.validate_layout_change(*old_child)?;
            }
        }
        removed_children.extend(old_children.into_iter().flatten());
    } else {
        children_by_name(snapshot, &mut children, &mut removed_children)?;
    }

    let mut fs_snapshot = FsSnapshot::new();

    if let Some(old_ref) = snapshot.old {
        let new_hash = hash_json_instance(snapshot.project(), snapshot.new_tree(), snapshot.new)
            .expect("new Instance should be hashable");
        let old_hash = hash_json_instance(snapshot.project(), snapshot.old_tree(), old_ref)
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
                } else if !keeps_directory_layout(snapshot, *new_child_ref, old_child) {
                    snapshot.validate_layout_change(old_child)?;
                    let fresh = snapshot.with_joined_path(*new_child_ref, None)?;
                    children.push(snapshot.with_fresh_path(fresh.path, fresh.new, old_child.id()));
                    removed_children.push(old_child);
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

    use crate::syncback::sanitize_stem;

    fn directory_syncback_fixture(
        source: VfsSnapshot,
        rules: &str,
    ) -> (Vfs, crate::Project, memofs::test_support::RecordingHandle) {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(format!(
                        r#"{{"name":"Test","tree":{{"$path":"Source"}},"syncbackRules":{rules}}}"#
                    )),
                ),
                ("Source", source),
            ]),
        )
        .unwrap();
        let (backend, recording) = memofs::test_support::RecordingBackend::new(imfs);
        let vfs = Vfs::new(backend);
        vfs.set_watch_enabled(false);
        let project = crate::Project::load_exact(
            &vfs,
            Path::new("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        (vfs, project, recording)
    }

    fn directory_syncback_tree(vfs: &Vfs, project: &crate::Project) -> crate::snapshot::RojoTree {
        crate::snapshot::RojoTree::new(
            snapshot_from_vfs(&InstanceContext::default(), vfs, &project.file_location)
                .unwrap()
                .unwrap(),
        )
    }

    #[test]
    fn relayout_releases_only_exact_paired_sidecars() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        let model = WeakDom::new(InstanceBuilder::new("Model").with_name("Twin"));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &model, &[model.root_ref()]).unwrap();
        for extension in ["json", "jsonc"] {
            for (source_stem, ownership, directories) in [
                ("Twin", "paired"),
                ("TWIN", "paired"),
                ("TWIN", "case-variant"),
                ("TWIN", "shared"),
            ]
            .into_iter()
            .flat_map(|(stem, owner)| {
                [false, true].map(move |directories| (stem, owner, directories))
            }) {
                let mut entries = vec![
                    (
                        format!("{source_stem}.rbxm"),
                        VfsSnapshot::file(bytes.clone()),
                    ),
                    (
                        format!("{source_stem}.meta.{extension}"),
                        VfsSnapshot::file(r#"{"id":"paired-owner","name":"Twin"}"#),
                    ),
                ];
                if ownership == "case-variant" {
                    entries.push((
                        format!("Twin.meta.{extension}"),
                        VfsSnapshot::file(r#"{"id":"unrelated-owner"}"#),
                    ));
                } else if ownership == "shared" {
                    // Both sources read the exact same sidecar, but only the
                    // paired model justifies releasing its old representation.
                    entries.push(("TWIN.luau".to_owned(), VfsSnapshot::file("return 1")));
                }
                let (vfs, project, recording) = directory_syncback_fixture(
                    VfsSnapshot::dir(entries),
                    &format!(
                        r#"{{"instancesAsDirectories":{directories},"modelTrees":[{{"pattern":"Twin","format":"rbxmx"}}]}}"#
                    ),
                );
                let before = vfs.snapshot_tree(Path::new("/project")).unwrap();
                let mut tree = directory_syncback_tree(&vfs, &project);
                let incoming = WeakDom::new(
                    InstanceBuilder::new("Folder")
                        .with_name("Test")
                        .with_child(InstanceBuilder::new("Model").with_name("Twin")),
                );
                recording.start();
                let staged = crate::syncback::syncback_loop(&vfs, &mut tree, incoming, &project);
                if ownership != "paired" {
                    let error = staged.err().unwrap_or_else(|| panic!(
                        "An unrelated or shared sidecar must block relayout for {source_stem}, {ownership}, directory mode {directories}, {extension}"
                    ));
                    assert!(
                        error
                            .to_string()
                            .contains("another source owns its destination")
                            || (!directories
                                && ownership == "shared"
                                && error.to_string().contains("duplicated on the file system")),
                        "{error:#}"
                    );
                    assert_eq!(vfs.snapshot_tree(Path::new("/project")).unwrap(), before);
                    assert!(recording.stop().keys().all(|call| matches!(
                        call.operation,
                        "read"
                            | "read_dir"
                            | "metadata"
                            | "exists"
                            | "canonicalize"
                            | "snapshot_tree"
                    )));
                } else {
                    staged.unwrap().write_to_vfs("/project", &vfs).unwrap();
                    let reloaded = directory_syncback_tree(&vfs, &project);
                    let child = reloaded
                        .get_instance(reloaded.root().children()[0])
                        .unwrap();
                    assert_eq!(child.name(), "Twin");
                    assert_eq!(child.metadata().middleware, Some(Middleware::Rbxmx));
                    assert_eq!(
                        child.metadata().specified_id.as_ref().unwrap().to_string(),
                        "paired-owner"
                    );
                }
            }
        }
    }

    #[test]
    fn default_layout_class_replacements_use_fresh_localization_initializers() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for old_class in ["LocalizationTable", "Folder"] {
            let new_class = if old_class == "Folder" {
                "LocalizationTable"
            } else {
                "Folder"
            };
            let temp = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            vfs.create_dir_all(root.join("Source/Holder/Table/Child"))
                .unwrap();
            if old_class == "LocalizationTable" {
                vfs.write(
                    root.join("Source/Holder/Table/init.csv"),
                    "Key,Source\nhello,Hello\n",
                )
                .unwrap();
            }
            let project_path = root.join("default.project.json");
            vfs.write(
                &project_path,
                r#"{"name":"Test","tree":{"$path":"Source"}}"#,
            )
            .unwrap();
            let project = crate::Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
            for _ in 0..2 {
                let mut tree = directory_syncback_tree(&vfs, &project);
                let mut child = InstanceBuilder::new(new_class)
                    .with_name("Table")
                    .with_child(InstanceBuilder::new("Folder").with_name("Child"));
                if new_class == "LocalizationTable" {
                    child = child.with_property("Contents", "[]");
                }
                let incoming = WeakDom::new(
                    InstanceBuilder::new("Folder").with_name("Test").with_child(
                        InstanceBuilder::new("Folder")
                            .with_name("Holder")
                            .with_child(child),
                    ),
                );
                let staged =
                    crate::syncback::syncback_loop(&vfs, &mut tree, incoming, &project).unwrap();
                staged.write_to_vfs(&root, &vfs).unwrap();
                let reloaded = directory_syncback_tree(&vfs, &project);
                let holder = reloaded
                    .get_instance(reloaded.root().children()[0])
                    .unwrap();
                assert_eq!(holder.children().len(), 1);
                let child = reloaded.get_instance(holder.children()[0]).unwrap();
                assert_eq!(child.class_name(), ustr(new_class));
                assert_eq!(child.children().len(), 1);
                assert_eq!(
                    vfs.metadata(root.join("Source/Holder/Table/init.csv"))
                        .with_not_found()
                        .unwrap()
                        .is_some(),
                    new_class == "LocalizationTable"
                );
                assert_eq!(
                    child.metadata().middleware,
                    Some(if new_class == "Folder" {
                        Middleware::Dir
                    } else {
                        Middleware::CsvDir
                    })
                );
            }
        }
    }

    #[test]
    fn case_variant_json_relayout_removes_only_the_actual_old_sidecar() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        let (vfs, project, _) = directory_syncback_fixture(
            VfsSnapshot::dir([
                (
                    "ROOT.model.json",
                    VfsSnapshot::file(r#"{"className":"Model"}"#),
                ),
                (
                    "ROOT.meta.jsonc",
                    VfsSnapshot::file(r#"{"id":"root-owner","name":"Root"}"#),
                ),
            ]),
            r#"{"instancesAsDirectories":true,"modelTrees":[{"pattern":"Root","format":"rbxm"}]}"#,
        );
        for _ in 0..2 {
            let mut tree = directory_syncback_tree(&vfs, &project);
            let incoming = WeakDom::new(
                InstanceBuilder::new("Folder")
                    .with_name("Test")
                    .with_child(InstanceBuilder::new("Model").with_name("Root")),
            );
            let staged =
                crate::syncback::syncback_loop(&vfs, &mut tree, incoming, &project).unwrap();
            staged.write_to_vfs("/project", &vfs).unwrap();
            let mut names: Vec<_> = vfs
                .read_dir("/project/Source")
                .unwrap()
                .map(|entry| entry.unwrap().path().file_name().unwrap().to_owned())
                .collect();
            names.sort();
            assert_eq!(names, ["Root.meta.json", "Root.rbxm"]);
            let reloaded = directory_syncback_tree(&vfs, &project);
            let child = reloaded
                .get_instance(reloaded.root().children()[0])
                .unwrap();
            assert_eq!(
                child.metadata().specified_id.as_ref().unwrap().to_string(),
                "root-owner"
            );
        }
    }

    fn names(children: &[InstanceSnapshot]) -> Vec<(String, String)> {
        children
            .iter()
            .map(|child| (child.name.to_string(), child.class_name.to_string()))
            .collect()
    }

    fn numbered(items: &[(u64, &str, &str)]) -> Vec<(u64, InstanceSnapshot)> {
        items
            .iter()
            .map(|(number, name, class)| {
                (
                    *number,
                    InstanceSnapshot::new().name(*name).class_name(*class),
                )
            })
            .collect()
    }

    #[test]
    fn replacement_stems_keep_exact_ordinals_or_report_ownership_conflicts() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for name in ["Twin", "twin", "Twin.__100", "Odd?.__100"] {
            for middleware in [
                Middleware::Dir,
                Middleware::Rbxm,
                Middleware::Rbxmx,
                Middleware::JsonModel,
            ] {
                let dom = WeakDom::new(InstanceBuilder::new("Folder").with_name(name));
                let base = sanitize_stem(name, "Folder");
                for number in [1, 3, 8, u64::MAX] {
                    let expected = if number == 1 {
                        base.clone()
                    } else {
                        format!("{base}.__{number}")
                    };
                    assert_eq!(
                        replacement_directory_stem(dom.root(), middleware, number, &HashSet::new())
                            .unwrap(),
                        expected
                    );
                    let extensions: &[&str] = match middleware {
                        Middleware::Dir => &[""],
                        Middleware::Rbxm => &[".rbxm", ".meta.json", ".meta.jsonc"],
                        Middleware::Rbxmx => &[".rbxmx", ".meta.json", ".meta.jsonc"],
                        Middleware::JsonModel => &[".model.json", ".meta.json", ".meta.jsonc"],
                        _ => unreachable!(),
                    };
                    for extension in extensions {
                        let reserved =
                            HashSet::from([format!("{expected}{extension}").to_lowercase()]);
                        let error =
                            replacement_directory_stem(dom.root(), middleware, number, &reserved)
                                .unwrap_err();
                        assert!(error
                            .downcast_ref::<crate::syncback::OwnershipConflict>()
                            .is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn duplicate_siblings_go_back_in_numbered_order() {
        // `Twin.__2/` has a key before `Twin.luau`, listing the second Twin first.
        let ordered = order_duplicate_siblings(numbered(&[
            (1, "Alpha", "Folder"),
            (2, "Twin", "Folder"),
            (1, "Twin", "ModuleScript"),
            (1, "Zeta", "Folder"),
        ]));
        assert_eq!(
            names(&ordered),
            [
                ("Alpha".into(), "Folder".into()),
                ("Twin".into(), "ModuleScript".into()),
                ("Twin".into(), "Folder".into()),
                ("Zeta".into(), "Folder".into()),
            ]
        );
    }

    #[test]
    fn duplicate_siblings_are_grouped_regardless_of_case() {
        let ordered = order_duplicate_siblings(numbered(&[
            (3, "twin", "Part"),
            (1, "Other", "Folder"),
            (1, "Twin", "Part"),
            (2, "TWIN", "Part"),
        ]));
        let order: Vec<_> = names(&ordered).into_iter().map(|(name, _)| name).collect();
        assert_eq!(order, ["Twin", "Other", "TWIN", "twin"]);
    }

    #[test]
    fn unique_children_keep_their_order() {
        for count in 0..5 {
            let items: Vec<_> = (0..count)
                .map(|i| (1, ["E", "D", "C", "B", "A"][i], "Folder"))
                .collect();
            let ordered = order_duplicate_siblings(numbered(&items));
            let order: Vec<_> = names(&ordered).into_iter().map(|(name, _)| name).collect();
            let expected: Vec<_> = items.iter().map(|(_, name, _)| name.to_string()).collect();
            assert_eq!(order, expected);
        }
    }

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

    #[test]
    fn directory_pairing_keeps_legacy_duplicates_in_sibling_order() {
        let old = keys(&[
            ("Twin", "Folder"),
            ("Twin", "ModuleScript"),
            ("Twin", "Folder"),
            ("Twin", "ModuleScript"),
        ]);
        let new = keys(&[
            ("Unrelated", "Folder"),
            ("Twin", "Folder"),
            ("Twin", "ModuleScript"),
            ("Twin", "Folder"),
            ("Twin", "ModuleScript"),
            ("Twin", "Folder"),
        ]);
        assert_eq!(
            pair_directory_indices(&new, &old),
            [None, Some(0), Some(1), Some(2), Some(3), None]
        );
    }

    #[test]
    fn directory_pairing_follows_name_and_class_without_position_preference() {
        let old = keys(&[
            ("Other", "Folder"),
            ("Twin", "Folder"),
            ("Twin", "ModuleScript"),
            ("Twin", "Folder"),
        ]);
        let new = keys(&[
            ("Twin", "Folder"),
            ("Twin", "Folder"),
            ("Other", "Folder"),
            ("Twin", "ModuleScript"),
        ]);
        assert_eq!(
            pair_directory_indices(&new, &old),
            [Some(1), Some(3), Some(0), Some(2)]
        );
    }

    #[test]
    fn directory_pairing_never_takes_another_name_or_class_metadata() {
        let old = keys(&[("Twin", "Folder"), ("Other", "ModuleScript")]);
        let new = keys(&[("Twin", "ModuleScript"), ("Other", "Folder")]);
        assert_eq!(pair_directory_indices(&new, &old), [None, None]);
    }

    #[test]
    fn directory_replacements_require_unconsumed_same_name_same_path_ownership() {
        let old = keys(&[
            ("Twin", "Folder"),
            ("Twin", "Configuration"),
            ("Other", "Folder"),
        ]);
        let new = keys(&[("Twin", "Model"), ("Twin", "Tool"), ("Wrong", "Model")]);
        let paths = ["twin", "twin.__2", "other"].map(String::from);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(&new, &old, &paths, &paths, &mut pairs);
        assert_eq!(pairs, [Some(0), Some(1), None]);

        // Reordering changed classes still follows exact paths, not old class queues.
        let reordered = keys(&[("Twin", "Tool"), ("Twin", "Model"), ("Other", "Model")]);
        let mut pairs = pair_directory_indices(&reordered, &old);
        pair_directory_replacements(&reordered, &old, &paths, &paths, &mut pairs);
        assert_eq!(pairs, [Some(0), Some(1), Some(2)]);

        // Sparse legacy paths do not justify lending metadata by name alone.
        let sparse = ["twin~3", "twin~8", "other"].map(String::from);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(&new, &old, &paths, &sparse, &mut pairs);
        assert_eq!(pairs, [None, None, None]);
    }

    #[test]
    fn directory_replacements_do_not_steal_paired_or_retained_destinations() {
        let old = keys(&[("Twin", "Folder"), ("Twin", "Model")]);
        let new = keys(&[("Twin", "Model"), ("Twin", "Tool")]);
        let old_paths = ["twin", "twin.__2"].map(String::from);
        let outputs = ["twin.__2", "twin.__2"].map(String::from);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(&new, &old, &outputs, &old_paths, &mut pairs);
        assert_eq!(pairs, [Some(1), None]);

        // The first old source is consumed even when a relayout moves its output.
        let new = keys(&[("Twin", "Folder"), ("Twin", "Tool")]);
        let outputs = ["elsewhere", "twin"].map(String::from);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(&new, &old, &outputs, &old_paths, &mut pairs);
        assert_eq!(pairs, [Some(0), None]);
    }

    #[test]
    fn directory_replacements_reject_ambiguous_physical_metadata() {
        let old = keys(&[("Twin", "Folder"), ("Twin", "Configuration")]);
        let new = keys(&[("Twin", "Model")]);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(
            &new,
            &old,
            &["twin".into()],
            &["twin".into(), "twin".into()],
            &mut pairs,
        );
        assert_eq!(pairs, [None]);

        let new = keys(&[("Twin", "Folder"), ("Twin", "Model")]);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(
            &new,
            &old,
            &["elsewhere".into(), "twin".into()],
            &["twin".into(), "twin".into()],
            &mut pairs,
        );
        assert_eq!(pairs, [Some(0), None]);
    }

    #[test]
    fn directory_replacements_follow_only_justified_normalized_renames() {
        for (before, after, path) in [("Twin", "twin", "twin.luau"), ("a?", "a*", "a_.luau")] {
            let old = keys(&[(before, "ModuleScript")]);
            let new = keys(&[(after, "ModuleScript")]);
            let mut pairs = pair_directory_indices(&new, &old);
            pair_directory_replacements(&new, &old, &[path.into()], &[path.into()], &mut pairs);
            assert_eq!(pairs, [Some(0)]);
        }

        let old = keys(&[("Twin", "ModuleScript"), ("Twin", "ModuleScript")]);
        let new = keys(&[("twin", "ModuleScript"), ("Twin", "ModuleScript")]);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(
            &new,
            &old,
            &["twin.luau".into(), "twin.luau".into()],
            &["twin.luau".into(), "twin~8.luau".into()],
            &mut pairs,
        );
        assert_eq!(pairs, [None, Some(0)]);

        let old = keys(&[("a?", "Folder"), ("a*", "Folder")]);
        let new = keys(&[("a:", "Folder")]);
        let mut pairs = pair_directory_indices(&new, &old);
        pair_directory_replacements(
            &new,
            &old,
            &["a_".into()],
            &["a_".into(), "a_".into()],
            &mut pairs,
        );
        assert_eq!(pairs, [None]);
    }

    #[test]
    fn directory_append_stems_respect_sparse_floors_pending_paths_and_exhaustion() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for name in ["Twin", "twin", "TWIN", "Twin.__100"] {
            let dom = WeakDom::new(InstanceBuilder::new("ModuleScript").with_name(name));
            let base = sanitize_stem(name, "ModuleScript");
            let reserved = HashSet::from([
                format!("{base}.__9.meta.json").to_lowercase(),
                format!("{base}.__10.meta.jsonc").to_lowercase(),
                format!("{base}.__11.luau").to_lowercase(),
            ]);
            assert_eq!(
                appended_directory_stem(dom.root(), Middleware::ModuleScript, 8, &reserved)
                    .unwrap(),
                format!("{}.__12", sanitize_stem(name, "ModuleScript"))
            );
            assert!(appended_directory_stem(
                dom.root(),
                Middleware::ModuleScript,
                u64::MAX,
                &reserved
            )
            .is_err());
        }
        let dom = WeakDom::new(InstanceBuilder::new("Folder").with_name("Twin"));
        assert_eq!(
            appended_directory_stem(dom.root(), Middleware::Dir, u64::MAX - 1, &HashSet::new())
                .unwrap(),
            format!("Twin.__{}", u64::MAX)
        );
        assert!(appended_directory_stem(
            dom.root(),
            Middleware::Dir,
            u64::MAX - 1,
            &HashSet::from([format!("twin.__{}", u64::MAX)])
        )
        .is_err());
    }

    #[test]
    fn directory_pairing_preserves_canonical_script_migration_allocation() {
        let old = keys(&[
            ("Effect", "LocalScript"),
            ("Effect", "LocalScript"),
            ("Twin", "ModuleScript"),
            ("Twin", "ModuleScript"),
        ]);
        let new = old.clone();
        let old_paths = [
            "effect.client.luau",
            "effect.local.luau",
            "twin.luau",
            "twin~3.luau",
        ]
        .map(String::from);
        let new_paths = [
            "effect.local.luau",
            "effect.__2.local.luau",
            "twin.luau",
            "twin.__2.luau",
        ]
        .map(String::from);
        let mut pairs = pair_directory_indices(&new, &old);
        let migrating = HashSet::from([("Effect", ustr("LocalScript"))]);
        pair_script_migration_indices(&new, &old, &new_paths, &old_paths, &migrating, &mut pairs);
        assert_eq!(pairs, [Some(1), None, Some(2), Some(3)]);
    }

    #[test]
    fn directory_layout_changes_displace_only_conflicting_retained_sources() {
        let fresh = ["fx.rbxm", "fx.rbxm.__2", "third", "safe"].map(String::from);
        let kept = [
            None,
            Some("fx.rbxm".into()),
            Some("fx.rbxm.__2".into()),
            Some("safe~8".into()),
        ];
        assert_eq!(
            directory_fresh_outputs(&fresh, &kept, &[true, false, false, false]),
            [true, true, true, false]
        );
        assert_eq!(
            directory_fresh_outputs(&fresh, &kept, &[false; 4]),
            [true, false, false, false]
        );
        let kept = fresh.iter().cloned().map(Some).collect::<Vec<_>>();
        assert_eq!(
            directory_fresh_outputs(&fresh, &kept, &[false; 4]),
            [false; 4]
        );
        // Merely sharing another sibling's freshly allocated stem is harmless
        // when both old sources can stay where they are.
        let kept = [
            Some("fx.rbxm.__2".into()),
            Some("fx.rbxm".into()),
            Some("third~8".into()),
            Some("safe".into()),
        ];
        assert_eq!(
            directory_fresh_outputs(&fresh, &kept, &[false; 4]),
            [false; 4]
        );
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
    fn numbered_exports_reload_in_order_with_legacy_and_mixed_layouts() {
        for (marker, name, extension) in [".__", "~"].into_iter().flat_map(|marker| {
            ["bucket", "Part.__2", "Part__10", "Odd?.__10"]
                .into_iter()
                .flat_map(move |name| {
                    ["json", "jsonc"]
                        .into_iter()
                        .map(move |extension| (marker, name, extension))
                })
        }) {
            let base = sanitize_stem(name, "ModuleScript");
            let mut entries = Vec::new();
            for number in 1..=14 {
                let stem = if number == 1 {
                    base.clone()
                } else {
                    format!("{base}{marker}{number}")
                };
                let source = VfsSnapshot::file(format!("return {number}"));
                let meta = VfsSnapshot::file(format!(r#"{{"name":"{name}"}}"#));
                if number % 2 == 0 {
                    entries.push((
                        stem,
                        VfsSnapshot::dir([
                            ("init.luau".to_owned(), source),
                            (format!("init.meta.{extension}"), meta),
                        ]),
                    ));
                } else {
                    entries.push((format!("{stem}.luau"), source));
                    entries.push((format!("{stem}.meta.{extension}"), meta));
                }
            }
            let mut imfs = InMemoryFs::new();
            imfs.load_snapshot("/root", VfsSnapshot::dir(entries))
                .unwrap();
            let vfs = Vfs::new(imfs);
            vfs.set_watch_enabled(false);
            for workers in [1, 4] {
                let loaded = rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .build()
                    .unwrap()
                    .install(|| {
                        snapshot_from_vfs(&InstanceContext::default(), &vfs, Path::new("/root"))
                            .unwrap()
                            .unwrap()
                    });
                assert_eq!(loaded.children.len(), 14);
                for (index, child) in loaded.children.iter().enumerate() {
                    assert_eq!(child.name, name);
                    assert_eq!(child.class_name, ustr("ModuleScript"));
                    assert_eq!(
                        child.properties[&ustr("Source")],
                        Variant::String(format!("return {}", index + 1))
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
