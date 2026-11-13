use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::Context;
use memofs::{DirEntry, Vfs};
use rayon::prelude::*;

use crate::{
    snapshot::{
        is_path_ignored, InstanceContext, InstanceMetadata, InstanceSnapshot, InstanceWithMeta,
        InstigatingSource,
    },
    syncback::{
        hash_instance, instances_as_directories, unique_child_stems, FsSnapshot, SyncbackReturn,
        SyncbackSnapshot,
    },
};

use super::{meta_file::DirectoryMetadata, snapshot_from_vfs};

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
    let entries = match entries {
        Some(entries) => entries,
        None => vfs.read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?,
    };
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|child| !is_path_ignored(&context.path_ignore_rules, child.path()))
        .collect();
    let snapshot_child = |entry: &DirEntry| snapshot_from_vfs(context, vfs, entry.path());
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

    let normalized_path = vfs.canonicalize(path)?;
    let relevant_paths = vec![
        normalized_path.clone(),
        // TODO: We shouldn't need to know about Lua existing in this
        // middleware. Should we figure out a way for that function to add
        // relevant paths to this middleware?
        normalized_path.join("init.lua"),
        normalized_path.join("init.luau"),
        normalized_path.join("init.server.lua"),
        normalized_path.join("init.server.luau"),
        normalized_path.join("init.client.lua"),
        normalized_path.join("init.client.luau"),
        normalized_path.join("init.legacy.lua"),
        normalized_path.join("init.legacy.luau"),
        normalized_path.join("init.local.lua"),
        normalized_path.join("init.local.luau"),
        normalized_path.join("init.plugin.lua"),
        normalized_path.join("init.plugin.luau"),
        normalized_path.join("init.csv"),
    ];

    let snapshot = InstanceSnapshot::new()
        .name(name)
        .class_name("Folder")
        .children(snapshot_children)
        .metadata(
            InstanceMetadata::new()
                .instigating_source(path)
                .relevant_paths(relevant_paths)
                .context(context),
        );

    Ok(Some(snapshot))
}

pub fn syncback_dir<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();

    let mut dir_syncback = syncback_dir_no_meta(snapshot)?;

    let mut meta = DirectoryMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    if let Some(meta) = &mut meta {
        if new_inst.class != "Folder" {
            meta.class_name = Some(new_inst.class);
        }

        if !meta.is_empty() {
            dir_syncback.fs_snapshot.add_file(
                snapshot.path.join("init.meta.json"),
                crate::json::to_vec_pretty(&meta)
                    .context("could not serialize new init.meta.json")?,
            );
        }
    }

    let metadata_empty = meta
        .as_ref()
        .map(DirectoryMetadata::is_empty)
        .unwrap_or_default();
    if new_inst.children().is_empty() && metadata_empty {
        dir_syncback
            .fs_snapshot
            .add_file(snapshot.path.join(EMPTY_DIR_KEEP_NAME), Vec::new())
    }

    Ok(dir_syncback)
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
