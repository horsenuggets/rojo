use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use anyhow::Context;
use memofs::{DirEntry, IoResultExt as _, Vfs};
use rayon::prelude::*;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr,
};

use crate::{
    snapshot::{
        is_path_ignored, InstanceContext, InstanceMetadata, InstanceSnapshot, InstanceWithMeta,
        InstigatingSource,
    },
    syncback::{
        filter_properties, hash_instance, instances_as_directories, sibling_order_key,
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

    let meta_path = snapshot.path.join("init.meta.json");
    if let Some(meta) = &mut meta {
        if new_inst.class != "Folder" {
            meta.class_name = Some(new_inst.class);
        }
        if inline {
            meta.children = inline_children(snapshot, snapshot.new, snapshot.old);
        }

        if !meta.is_empty() {
            dir_syncback.fs_snapshot.add_file(
                &meta_path,
                crate::json::to_vec_pretty(&meta)
                    .context("could not serialize new init.meta.json")?,
            );
        }
    }

    let metadata_empty = meta
        .as_ref()
        .map(DirectoryMetadata::is_empty)
        .unwrap_or_default();

    // Children that were stored inline are now written as their own files, so
    // the old entries must not survive to load a second copy of each of them.
    if !inline && has_inline_children(snapshot) {
        let jsonc_path = snapshot.path.join("init.meta.jsonc");
        let mut stale = vec![jsonc_path];
        if metadata_empty {
            stale.push(meta_path);
        }
        for path in stale {
            if snapshot
                .vfs()
                .metadata(&path)
                .with_not_found()?
                .is_some_and(|metadata| metadata.is_file())
            {
                dir_syncback.fs_snapshot.remove_file(path);
            }
        }
    }

    if new_inst.children().is_empty() && metadata_empty {
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
    !snapshot.ignore_tree().is_some_and(|ignored| {
        ignored
            .iter()
            .any(|tree| inst_path.is_empty() || tree.starts_with(&prefix))
    })
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
            let mut values = filter_properties(snapshot.project(), instance)
                .into_iter()
                .collect::<Vec<_>>();
            // Hash map order would reshuffle the file on every rewrite.
            values.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
            let (properties, attributes) = serialize_values(&instance.class, values);
            let id = old_child
                .and_then(|old| old.metadata().specified_id.as_ref())
                .map(ToString::to_string)
                // IDs given through the `Rojo_Id` attribute are already saved.
                .filter(|id| rojo_id_attribute(instance) != Some(id.as_str()));

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
        .map(|child| snapshot.get_new_instance(*child).unwrap())
        .collect();
    let mut taken = vec![false; old_children.len()];
    let mut pairs = vec![None; new.len()];

    let mut claim = |pairs: &mut Vec<Option<usize>>, matches: &dyn Fn(usize, usize) -> bool| {
        for (index, pair) in pairs.iter_mut().enumerate() {
            if pair.is_some() {
                continue;
            }
            if let Some(old_index) = (0..old_children.len())
                .find(|&old_index| !taken[old_index] && matches(index, old_index))
            {
                taken[old_index] = true;
                *pair = Some(old_index);
            }
        }
    };
    let same_class =
        |index: usize, old_index: usize| new[index].class == old_children[old_index].class_name();
    let same_name =
        |index: usize, old_index: usize| new[index].name == old_children[old_index].name();
    claim(&mut pairs, &|index, old_index| {
        index == old_index && same_class(index, old_index) && same_name(index, old_index)
    });
    claim(&mut pairs, &|index, old_index| {
        same_class(index, old_index) && same_name(index, old_index)
    });
    claim(&mut pairs, &|index, old_index| {
        index == old_index && same_class(index, old_index)
    });

    pairs
        .into_iter()
        .map(|pair| pair.map(|old_index| old_children[old_index]))
        .collect()
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
