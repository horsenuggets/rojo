//! Serializes a subtree created in Studio as new files.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::Context;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, InstanceBuilder, WeakDom,
};

use crate::{
    rojo_ref::{RojoRef, REF_ID_ATTRIBUTE_NAME},
    snapshot::RojoTree,
    Project,
};

use super::{
    file_names::{directory_middleware_for_instance, free_child_stem},
    get_property_filter, inline, link_scoped_referents,
    model_trees::{CompiledModelTreeRules, ModelTrees},
    ref_properties::attribute_ids,
    syncback_snapshot, FsSnapshot, SyncbackData, SyncbackSnapshot,
};

/// The files a subtree created in Studio is stored in.
pub struct NewSubtree {
    pub fs_snapshot: FsSnapshot,
    /// The path the subtree's root is read back from.
    pub root_path: PathBuf,
}

/// Serializes `model`'s only top-level instance as a new child of `parent`,
/// with files inside `directory`, the directory that stores `parent`'s
/// children. `existing` holds the lowercased names of the entries already in
/// `directory`, which the new files must not take.
///
/// The output is the same as a full syncback would write for the subtree, so
/// the project's naming, layout, and ignore rules all apply. Nothing is written
/// to disk.
pub fn syncback_new_subtree(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    parent: Ref,
    directory: &Path,
    existing: &HashSet<String>,
    mut model: WeakDom,
) -> anyhow::Result<NewSubtree> {
    let top_level = model.root().children().to_vec();
    anyhow::ensure!(
        top_level.len() == 1,
        "A created subtree must have exactly one root, not {}",
        top_level.len()
    );
    let subtree = top_level[0];

    // Project rules match paths from the DataModel down, so the subtree is
    // placed under stand-ins for the parent and its ancestors.
    let mut ancestry = Vec::new();
    let mut current = parent;
    while current != old_tree.get_root_id() {
        let instance = old_tree
            .get_instance(current)
            .context("The parent is no longer known")?;
        ancestry.push((instance.class_name(), instance.name().to_owned()));
        current = instance.parent();
    }
    let root = old_tree.root();
    let mut new_tree = WeakDom::new(InstanceBuilder::new(root.class_name()).with_name(root.name()));
    let mut destination = new_tree.root_ref();
    for (class, name) in ancestry.into_iter().rev() {
        destination = new_tree.insert(destination, InstanceBuilder::new(class).with_name(name));
    }
    model.transfer(subtree, &mut new_tree, destination);
    link_scoped_referents(project, &mut new_tree)?;

    let parent_instance = old_tree.get_instance(parent).unwrap();
    let rules = CompiledModelTreeRules::new(
        project
            .syncback_rules
            .as_ref()
            .map(|rules| rules.model_trees.as_slice())
            .unwrap_or_default(),
        &project.tree,
    )?;
    let model_trees = ModelTrees::find(&rules, &new_tree);
    let script_containers = inline::script_containers(&new_tree);
    let attribute_ids = attribute_ids(&new_tree);
    let parent_snapshot = SyncbackSnapshot {
        data: SyncbackData {
            vfs,
            old_tree,
            new_tree: &new_tree,
            project,
            scoped: false,
            context: Some(&parent_instance.metadata().context),
            script_containers: Some(&script_containers),
            model_trees: Some(&model_trees),
            attribute_ids: &attribute_ids,
        },
        old: Some(parent),
        new: destination,
        path: directory.to_path_buf(),
        middleware: None,
    };

    let child = new_tree.get_by_ref(subtree).unwrap();
    let middleware = model_trees
        .middleware(subtree)
        .unwrap_or_else(|| directory_middleware_for_instance(child));
    let stem = free_child_stem(child, middleware, existing);
    let root_snapshot = parent_snapshot.with_stem(directory, subtree, None, &stem);

    let ignore_patterns = project
        .syncback_rules
        .as_ref()
        .map(|rules| rules.compile_globs())
        .transpose()?;
    let relayout = HashSet::new();
    let mut fs_snapshot = FsSnapshot::new();
    let mut owners = HashMap::new();
    let (mut pending, root_path) = syncback_snapshot(
        root_snapshot,
        &ignore_patterns,
        project,
        &relayout,
        &mut fs_snapshot,
        &mut owners,
    )?
    .context("The project's syncback rules exclude this instance")?;
    while let Some(snapshot) = pending.pop() {
        if let Some((children, _)) = syncback_snapshot(
            snapshot,
            &ignore_patterns,
            project,
            &relayout,
            &mut fs_snapshot,
            &mut owners,
        )? {
            pending.extend(children);
        }
    }

    anyhow::ensure!(
        fs_snapshot.removed_paths().is_empty(),
        "Creating an instance cannot remove existing files"
    );
    for path in fs_snapshot.added_paths() {
        anyhow::ensure!(
            path != directory && path.starts_with(directory),
            "Creating an instance can only write inside its parent's directory"
        );
    }

    Ok(NewSubtree {
        fs_snapshot,
        root_path,
    })
}

/// Removes the properties the project's syncback rules ignore from every
/// instance in `model`, like a full syncback leaves them out.
pub fn drop_ignored_properties(project: &Project, model: &mut WeakDom) {
    let mut pending = model.root().children().to_vec();
    while let Some(referent) = pending.pop() {
        let instance = model.get_by_ref_mut(referent).unwrap();
        pending.extend(instance.children().iter().copied());
        if let Some(ignored) = get_property_filter(project, instance) {
            for property in ignored {
                instance.properties.remove(&property);
            }
        }
    }
}

/// Removes the `Rojo_Id` attribute from instances in `model` whose ID another
/// instance already holds. Duplicating an instance in Studio copies the
/// attribute, and two instances with one ID would break every reference to it.
/// The `moving` subtrees are what `model` holds, so their IDs aren't taken.
pub fn drop_duplicate_ids(model: &mut WeakDom, old_tree: &RojoTree, moving: &[Ref]) {
    let is_moving = |mut id: Ref| {
        while let Some(instance) = old_tree.get_instance(id) {
            if moving.contains(&id) {
                return true;
            }
            id = instance.parent();
        }
        false
    };
    let mut seen = HashSet::new();
    let mut pending = model.root().children().to_vec();
    while let Some(referent) = pending.pop() {
        let instance = model.get_by_ref_mut(referent).unwrap();
        pending.extend(instance.children().iter().copied());
        let Some(Variant::Attributes(attributes)) =
            instance.properties.get_mut(&ustr("Attributes"))
        else {
            continue;
        };
        // Binary models, which Studio sends, read string attributes back as
        // binary strings.
        let id = match attributes.get(REF_ID_ATTRIBUTE_NAME) {
            Some(Variant::String(id)) => id.clone(),
            Some(Variant::BinaryString(id)) => match std::str::from_utf8(id.as_ref()) {
                Ok(id) => id.to_owned(),
                Err(_) => continue,
            },
            _ => continue,
        };
        let taken = old_tree
            .specified_id_holders(&RojoRef::new(id.clone()))
            .iter()
            .any(|holder| !is_moving(*holder));
        if taken || !seen.insert(id.clone()) {
            attributes.remove(REF_ID_ATTRIBUTE_NAME);
        }
    }
}

#[cfg(test)]
mod test {
    use rbx_dom_weak::types::{Attributes, BinaryString};

    use super::*;
    use crate::snapshot::{InstanceMetadata, InstanceSnapshot};

    fn has_id(model: &WeakDom, name: &str) -> bool {
        let instance = model
            .descendants()
            .find(|instance| instance.name == name)
            .unwrap();
        matches!(
            instance.properties.get(&ustr("Attributes")),
            Some(Variant::Attributes(attributes)) if attributes.get(REF_ID_ATTRIBUTE_NAME).is_some()
        )
    }

    /// Walks every combination of how the ID is stored, whether another
    /// instance holds it, and whether that holder is what's being moved.
    #[test]
    fn duplicate_ids_are_dropped_unless_their_holder_is_moving() {
        for binary in [false, true] {
            for held in [false, true] {
                for moving in [false, true] {
                    let mut tree = RojoTree::new(InstanceSnapshot::new());
                    let root = tree.get_root_id();
                    let mut metadata = InstanceMetadata::new();
                    if held {
                        metadata.specified_id = Some(RojoRef::new("shared".into()));
                    }
                    let holder =
                        tree.insert_instance(root, InstanceSnapshot::new().metadata(metadata));

                    let value = if binary {
                        Variant::BinaryString(BinaryString::from(b"shared".to_vec()))
                    } else {
                        Variant::String("shared".into())
                    };
                    let mut attributes = Attributes::new();
                    attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), value);
                    let mut model = WeakDom::new(InstanceBuilder::new("Folder"));
                    let created = model.insert(
                        model.root_ref(),
                        InstanceBuilder::new("Folder").with_name("Created"),
                    );
                    for name in ["First", "Second"] {
                        model.insert(
                            created,
                            InstanceBuilder::new("Part")
                                .with_name(name)
                                .with_property("Attributes", attributes.clone()),
                        );
                    }

                    let moving_ids = if moving { vec![holder] } else { Vec::new() };
                    drop_duplicate_ids(&mut model, &tree, &moving_ids);

                    // Two copies in one model never both keep the ID.
                    let kept = ["First", "Second"]
                        .iter()
                        .filter(|name| has_id(&model, name))
                        .count();
                    let expected = if held && !moving { 0 } else { 1 };
                    assert_eq!(
                        kept, expected,
                        "binary {binary} held {held} moving {moving}"
                    );
                }
            }
        }
    }
}
