use std::path::Path;

use anyhow::Context;
use memofs::Vfs;

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

use super::{lua::metadata_exists, meta_file::AdjacentMetadata};

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
/// can't hold, or one shared with a sibling, is stored in the metadata.
pub(super) fn syncback_model_name(
    snapshot: &SyncbackSnapshot,
    fs_snapshot: &mut FsSnapshot,
) -> anyhow::Result<()> {
    let meta_path = snapshot
        .path
        .parent()
        .context("model files should have a parent directory")?
        .join(format!("{}.meta.json", snapshot.file_stem()));
    match snapshot.name_override() {
        Some(name) => fs_snapshot.add_file(
            meta_path,
            crate::json::to_vec_pretty(&serde_json::json!({ "name": name }))
                .context("could not serialize metadata")?,
        ),
        None if metadata_exists(snapshot.vfs(), &meta_path) => fs_snapshot.remove_file(meta_path),
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
