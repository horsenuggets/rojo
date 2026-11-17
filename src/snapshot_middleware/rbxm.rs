use std::path::Path;

use anyhow::Context;
use memofs::{IoResultExt, Vfs};

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

use super::meta_file::AdjacentMetadata;

#[profiling::function]
pub fn snapshot_rbxm(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let temp_tree = rbx_binary::from_reader(vfs.read(path)?.as_slice())
        .with_context(|| format!("Malformed rbxm file: {}", path.display()))?;

    let root_instance = temp_tree.root();
    let children = root_instance.children();

    if children.len() == 1 {
        let child = children[0];
        let mut snapshot = InstanceSnapshot::from_tree(temp_tree, child)
            .name(name)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(path)
                    .relevant_paths(vec![vfs.canonicalize(path)?])
                    .context(context),
            );

        AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;

        Ok(Some(snapshot))
    } else {
        anyhow::bail!(
            "Rojo currently only supports model files with one top-level instance.\n\n \
             Check the model file at path {}",
            path.display()
        );
    }
}

pub fn syncback_rbxm<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let inst = snapshot.new_inst();

    // Long-term, we probably want to have some logic for if this contains a
    // script. That's a future endeavor though.
    let mut serialized = Vec::new();
    rbx_binary::to_writer(&mut serialized, snapshot.new_tree(), &[inst.referent()])
        .context("failed to serialize new rbxm")?;

    let mut fs_snapshot = FsSnapshot::new().with_added_file(&snapshot.path, serialized);
    syncback_model_name(snapshot, &mut fs_snapshot)?;

    Ok(SyncbackReturn {
        fs_snapshot,
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

/// Keeps the adjacent metadata of a model file in step with its root's name.
/// The root is named after the file when it is read, so a name the file system
/// can't hold, or one shared with a sibling, is stored in the metadata. The
/// model file holds the root's properties and attributes, so the metadata
/// stops setting them, and its other fields are left as they are.
pub(super) fn syncback_model_name(
    snapshot: &SyncbackSnapshot,
    fs_snapshot: &mut FsSnapshot,
) -> anyhow::Result<()> {
    let directory = snapshot
        .path
        .parent()
        .context("model files should have a parent directory")?;
    let stem = snapshot.file_stem();
    let json_path = directory.join(format!("{stem}.meta.json"));
    let jsonc_path = directory.join(format!("{stem}.meta.jsonc"));
    let json = snapshot.vfs().read(&json_path).with_not_found()?;
    let jsonc = snapshot.vfs().read(&jsonc_path).with_not_found()?;

    // Both files are applied when reading, so rewriting only one of them can't
    // clear a name the other still sets.
    let (meta_path, existing) = match (json, jsonc) {
        (Some(_), Some(_)) => anyhow::bail!(
            "Both {} and {} hold this model's metadata, so it can't be safely rewritten",
            json_path.display(),
            jsonc_path.display()
        ),
        (None, Some(existing)) => (jsonc_path, Some(existing)),
        (existing, None) => (json_path, existing),
    };
    let existing = existing.as_deref().map(Vec::as_slice);
    let updated = AdjacentMetadata::for_model_root(existing, &meta_path, snapshot.name_override())?;
    match updated {
        Some(contents) if existing != Some(contents.as_slice()) => {
            fs_snapshot.add_file(meta_path, contents)
        }
        Some(_) => {}
        None if existing.is_some() => fs_snapshot.remove_file(meta_path),
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.rbxm",
            VfsSnapshot::file(include_bytes!("../../assets/test-folder.rbxm").to_vec()),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_rbxm(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.rbxm"),
            "foo",
        )
        .unwrap()
        .unwrap();

        assert_eq!(instance_snapshot.name, "foo");
        assert_eq!(instance_snapshot.class_name, "Folder");
        assert_eq!(instance_snapshot.children, Vec::new());

        // We intentionally don't assert on properties. rbx_binary does not
        // distinguish between String and BinaryString. The sample model was
        // created by Roblox Studio and has an empty BinaryString "Tags"
        // property that currently deserializes incorrectly.
        // See: https://github.com/Roblox/rbx-dom/issues/49
    }
}
