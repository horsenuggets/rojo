//! Defines the algorithm for applying generated patches.

use std::{
    collections::{HashMap, HashSet},
    mem::take,
};

use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Ustr,
};

use super::{
    patch::{AppliedPatchSet, AppliedPatchUpdate, PatchSet, PatchUpdate},
    InstanceSnapshot, RojoTree,
};
use crate::{multimap::MultiMap, RojoRef, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX};

/// Consumes the input `PatchSet`, applying all of its prescribed changes to the
/// tree and returns an `AppliedPatchSet`, which can be used to keep another
/// tree in sync with Rojo's.
#[profiling::function]
pub fn apply_patch_set(tree: &mut RojoTree, patch_set: PatchSet) -> AppliedPatchSet {
    try_apply_patch_set(tree, patch_set).expect("Invalid patch set")
}

/// Validates the complete final parent graph before making any changes.
/// Structural references are tree IDs, not transient snapshot IDs.
/// Updates and additions must target instances that survive the final removals.
/// Descendants explicitly moved out of a removed subtree survive with their identities.
pub fn validate_patch_set(tree: &RojoTree, patch_set: &PatchSet) -> anyhow::Result<()> {
    // Ordinary source edits must stay scoped to the edited instances, not walk
    // the entire project merely because structural patches are supported.
    if patch_set.removed_instances.is_empty()
        && patch_set
            .updated_instances
            .iter()
            .all(|update| update.changed_parent.is_none())
    {
        for update in &patch_set.updated_instances {
            anyhow::ensure!(
                tree.get_instance(update.id).is_some(),
                "Updated instance does not exist"
            );
        }
        for addition in &patch_set.added_instances {
            anyhow::ensure!(
                tree.get_instance(addition.parent_id).is_some(),
                "Addition parent does not exist"
            );
        }
        return Ok(());
    }
    let root = tree.get_root_id();
    let mut parents: HashMap<_, _> = tree
        .descendants(root)
        .map(|instance| (instance.id(), instance.parent()))
        .collect();
    let removed: HashSet<_> = patch_set.removed_instances.iter().copied().collect();
    for id in &removed {
        anyhow::ensure!(*id != root, "Cannot remove the tree root");
        anyhow::ensure!(parents.contains_key(id), "Removed instance does not exist");
    }
    let mut moved = HashSet::new();
    for update in &patch_set.updated_instances {
        anyhow::ensure!(
            parents.contains_key(&update.id),
            "Updated instance does not exist"
        );
        if let Some(parent) = update.changed_parent {
            anyhow::ensure!(update.id != root, "Cannot reparent the tree root");
            anyhow::ensure!(parents.contains_key(&parent), "Parent does not exist");
            anyhow::ensure!(moved.insert(update.id), "Duplicate reparent update");
            parents.insert(update.id, parent);
        }
    }

    // Resolve against the final graph, not update order or the old ancestry.
    let mut survives = HashMap::from([(root, true)]);
    for id in parents.keys().copied() {
        let mut path = Vec::new();
        let mut visiting = HashSet::new();
        let mut current = id;
        while !survives.contains_key(&current) {
            anyhow::ensure!(visiting.insert(current), "Patch would create a cycle");
            path.push(current);
            current = parents[&current];
        }
        let mut alive = survives[&current];
        for node in path.into_iter().rev() {
            alive &= !removed.contains(&node);
            survives.insert(node, alive);
        }
    }
    for update in &patch_set.updated_instances {
        anyhow::ensure!(
            survives[&update.id],
            "Updated instance is removed by the patch"
        );
    }
    for addition in &patch_set.added_instances {
        anyhow::ensure!(
            survives.get(&addition.parent_id) == Some(&true),
            "Addition parent does not survive the patch"
        );
    }
    Ok(())
}

/// Applies a valid patch atomically with respect to structural validation errors.
/// Invalid input leaves the tree and all its metadata indexes untouched.
pub fn try_apply_patch_set(
    tree: &mut RojoTree,
    patch_set: PatchSet,
) -> anyhow::Result<AppliedPatchSet> {
    validate_patch_set(tree, &patch_set)?;
    let mut context = PatchApplyContext::default();
    let root = tree.get_root_id();

    // Staging movers under the root breaks old ancestry without cloning or deleting
    // anything. A valid final graph can then be assembled in any update order.
    let moves: Vec<_> = patch_set
        .updated_instances
        .iter()
        .filter_map(|update| {
            let parent = update.changed_parent?;
            (tree.get_instance(update.id).unwrap().parent() != parent)
                .then_some((update.id, parent))
        })
        .collect();
    for &(id, _) in &moves {
        tree.reparent(id, root)?;
    }
    for (id, parent) in moves {
        tree.reparent(id, parent)?;
    }

    {
        profiling::scope!("removals");
        for removed_id in patch_set.removed_instances {
            // An earlier removal may already have removed this descendant.
            if tree.get_instance(removed_id).is_some() {
                apply_remove_instance(&mut context, tree, removed_id);
            }
        }
    }

    {
        profiling::scope!("additions");
        for add_patch in patch_set.added_instances {
            apply_add_child(&mut context, tree, add_patch.parent_id, add_patch.instance);
        }
    }

    {
        profiling::scope!("updates");
        // Updates need to be applied after additions, which reduces the complexity
        // of updates significantly.
        for update_patch in patch_set.updated_instances {
            apply_update_child(&mut context, tree, update_patch);
        }
    }

    Ok(finalize_patch_application(context, tree))
}

/// All of the ephemeral state needing during application of a patch.
#[derive(Default)]
struct PatchApplyContext {
    /// A map from transient snapshot IDs (generated by snapshot middleware) to
    /// instance IDs in the actual tree. These are both the same data type so
    /// that they fit into the same `Variant::Ref` type.
    ///
    /// At this point in the patch process, IDs in instance properties have been
    /// partially translated from 'snapshot space' into 'tree space' by the
    /// patch computation process. An ID not existing in this map means either:
    ///
    /// 1. The ID is already in tree space and refers to an instance that
    ///    existed in the tree before this patch was applied.
    ///
    /// 2. The ID if in snapshot space, but points to an instance that was not
    ///    part of the snapshot that was put through the patch computation
    ///    function.
    ///
    /// #2 should not occur in well-formed projects, but is indistinguishable
    /// from #1 right now. It could happen if two model files try to reference
    /// eachother.
    snapshot_id_to_instance_id: HashMap<Ref, Ref>,

    /// Tracks all of the instances added by this patch that have refs that need
    /// to be rewritten.
    has_refs_to_rewrite: HashSet<Ref>,

    /// Tracks all ref properties that were specified using attributes. This has
    /// to be handled after everything else is done just like normal referent
    /// properties.
    attribute_refs_to_rewrite: MultiMap<Ref, (Ustr, String)>,

    /// The current applied patch result, describing changes made to the tree.
    applied_patch_set: AppliedPatchSet,
}

/// Finalize this patch application, consuming the context, applying any
/// deferred property updates, and returning the finally applied patch set.
///
/// Ref properties from snapshots refer to eachother via snapshot ID. Some of
/// these properties are transformed when the patch is computed, notably the
/// instances that the patch computing method is able to pair up.
///
/// The remaining Ref properties need to be handled during patch application,
/// where we build up a map of snapshot IDs to instance IDs as they're created,
/// then apply properties all at once at the end.
#[profiling::function]
fn finalize_patch_application(context: PatchApplyContext, tree: &mut RojoTree) -> AppliedPatchSet {
    for id in context.has_refs_to_rewrite {
        // This should always succeed since instances marked as added in our
        // patch should be added without fail.
        let mut instance = tree
            .get_instance_mut(id)
            .expect("Invalid instance ID in deferred property map");

        for value in instance.properties_mut().values_mut() {
            if let Variant::Ref(referent) = value {
                if let Some(&instance_referent) = context.snapshot_id_to_instance_id.get(referent) {
                    *value = Variant::Ref(instance_referent);
                }
            }
        }
    }

    // This is to get around the fact that `RojoTre::get_specified_id` borrows
    // the tree as immutable, but we need to hold a mutable reference to it.
    // Not exactly elegant, but it does the job.
    let mut real_rewrites = Vec::new();
    for (id, map) in context.attribute_refs_to_rewrite {
        for (prop_name, prop_value) in map {
            if let Some(target) = tree.get_specified_id(&RojoRef::new(prop_value)) {
                real_rewrites.push((prop_name, Variant::Ref(target)))
            }
        }
        let mut instance = tree
            .get_instance_mut(id)
            .expect("Invalid instance ID in deferred attribute ref map");
        instance.properties_mut().extend(real_rewrites.drain(..));
    }

    context.applied_patch_set
}

fn apply_remove_instance(context: &mut PatchApplyContext, tree: &mut RojoTree, removed_id: Ref) {
    tree.remove(removed_id);
    context.applied_patch_set.removed.push(removed_id);
}

fn apply_add_child(
    context: &mut PatchApplyContext,
    tree: &mut RojoTree,
    parent_id: Ref,
    mut snapshot: InstanceSnapshot,
) {
    let snapshot_id = snapshot.snapshot_id;
    let children = take(&mut snapshot.children);

    // If an object we're adding has a non-null referent, we'll note this
    // instance down as needing to be revisited later.
    let has_refs = snapshot.properties.values().any(|value| match value {
        Variant::Ref(value) => value.is_some(),
        _ => false,
    });

    let id = tree.insert_instance(parent_id, snapshot);
    context.applied_patch_set.added.push(id);

    if has_refs {
        context.has_refs_to_rewrite.insert(id);
    }

    if snapshot_id.is_some() {
        context.snapshot_id_to_instance_id.insert(snapshot_id, id);
    }

    for child in children {
        apply_add_child(context, tree, id, child);
    }

    defer_ref_properties(tree, id, context);
}

fn apply_update_child(context: &mut PatchApplyContext, tree: &mut RojoTree, patch: PatchUpdate) {
    let mut applied_patch = AppliedPatchUpdate::new(patch.id);
    applied_patch.changed_parent = patch.changed_parent;

    if let Some(metadata) = patch.changed_metadata {
        tree.update_metadata(patch.id, metadata.clone());
        applied_patch.changed_metadata = Some(metadata);
    }

    let mut instance = match tree.get_instance_mut(patch.id) {
        Some(instance) => instance,
        None => {
            log::warn!(
                "Patch misapplication: Instance {:?}, referred to by update patch, did not exist.",
                patch.id
            );
            return;
        }
    };

    if let Some(name) = patch.changed_name {
        *instance.name_mut() = name.clone();
        applied_patch.changed_name = Some(name);
    }

    if let Some(class_name) = patch.changed_class_name {
        instance.set_class_name(class_name);
        applied_patch.changed_class_name = Some(class_name);
    }

    for (key, property_entry) in patch.changed_properties {
        match property_entry {
            // Ref values need to be potentially rewritten from snapshot IDs to
            // instance IDs if they referred to an instance that was created as
            // part of this patch.
            Some(Variant::Ref(referent)) => {
                // If our ID is not found in this map, then it either refers to
                // an existing instance NOT added by this patch, or there was an
                // error. See `PatchApplyContext::snapshot_id_to_instance_id`
                // for more info.
                let new_referent = context
                    .snapshot_id_to_instance_id
                    .get(&referent)
                    .copied()
                    .unwrap_or(referent);

                instance
                    .properties_mut()
                    .insert(key, Variant::Ref(new_referent));
            }
            Some(ref value) => {
                instance.properties_mut().insert(key, value.clone());
            }
            None => {
                instance.properties_mut().remove(&key);
            }
        }

        applied_patch.changed_properties.insert(key, property_entry);
    }

    defer_ref_properties(tree, patch.id, context);

    context.applied_patch_set.updated.push(applied_patch)
}

/// Calculates manually-specified Ref properties and marks them in the provided
/// `PatchApplyContext` to be rewritten at the end of the patch application
/// process.
///
/// Currently, this only uses attributes but it can easily handle rewriting
/// referents in other ways too!
fn defer_ref_properties(tree: &mut RojoTree, id: Ref, context: &mut PatchApplyContext) {
    let instance = tree
        .get_instance(id)
        .expect("Instances should exist when calculating deferred refs");
    let attributes = match instance.properties().get(&ustr("Attributes")) {
        Some(Variant::Attributes(attrs)) => attrs,
        _ => return,
    };

    let mut attr_id = None;
    for (attr_name, attr_value) in attributes.iter() {
        if attr_name == REF_ID_ATTRIBUTE_NAME {
            if let Variant::String(specified_id) = attr_value {
                attr_id = Some(RojoRef::new(specified_id.clone()));
            } else if let Variant::BinaryString(specified_id) = attr_value {
                if let Ok(str) = std::str::from_utf8(specified_id.as_ref()) {
                    attr_id = Some(RojoRef::new(str.to_string()))
                } else {
                    log::error!("Specified IDs must be valid UTF-8 strings.")
                }
            } else {
                log::warn!(
                    "Attribute {attr_name} is of type {:?} when it was \
                    expected to be a String",
                    attr_value.ty()
                )
            }
        }
        if let Some(prop_name) = attr_name.strip_prefix(REF_POINTER_ATTRIBUTE_PREFIX) {
            if let Variant::String(prop_value) = attr_value {
                context
                    .attribute_refs_to_rewrite
                    .insert(id, (ustr(prop_name), prop_value.clone()));
            } else if let Variant::BinaryString(prop_value) = attr_value {
                if let Ok(str) = std::str::from_utf8(prop_value.as_ref()) {
                    context
                        .attribute_refs_to_rewrite
                        .insert(id, (ustr(prop_name), str.to_string()));
                } else {
                    log::error!("IDs specified by referent property attributes must be valid UTF-8 strings.")
                }
            } else {
                log::warn!(
                    "Attribute {attr_name} is of type {:?} when it was \
                    expected to be a String",
                    attr_value.ty()
                )
            }
        }
    }
    if let Some(specified_id) = attr_id {
        tree.set_specified_id(id, specified_id);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::borrow::Cow;

    use rbx_dom_weak::{types::Variant, UstrMap};

    use super::super::PatchAdd;

    #[test]
    fn add_from_empty() {
        let _ = env_logger::try_init();

        let mut tree = RojoTree::new(InstanceSnapshot::new());

        let root_id = tree.get_root_id();

        let snapshot = InstanceSnapshot {
            snapshot_id: Ref::none(),
            metadata: Default::default(),
            name: Cow::Borrowed("Foo"),
            class_name: ustr("Bar"),
            properties: UstrMap::from_iter([(ustr("Baz"), Variant::Int32(5))]),
            children: Vec::new(),
        };

        let patch_set = PatchSet {
            added_instances: vec![PatchAdd {
                parent_id: root_id,
                instance: snapshot.clone(),
            }],
            ..Default::default()
        };

        apply_patch_set(&mut tree, patch_set);

        let root_instance = tree.get_instance(root_id).unwrap();
        let child_id = root_instance.children()[0];
        let child_instance = tree.get_instance(child_id).unwrap();

        assert_eq!(child_instance.name(), &snapshot.name);
        assert_eq!(child_instance.class_name(), snapshot.class_name);
        assert_eq!(child_instance.properties(), &snapshot.properties);
        assert!(child_instance.children().is_empty());
    }

    #[test]
    fn ordinary_updates_validate_every_target_before_mutation() {
        let mut tree = RojoTree::new(InstanceSnapshot::new().name("Unchanged"));
        let root = tree.get_root_id();
        let update = |id| PatchUpdate {
            id,
            changed_name: Some("Changed".into()),
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::default(),
            changed_metadata: None,
        };
        let patch = PatchSet {
            updated_instances: vec![update(root), update(Ref::none())],
            ..PatchSet::new()
        };
        assert!(try_apply_patch_set(&mut tree, patch).is_err());
        assert_eq!(tree.root().name(), "Unchanged");
    }

    #[test]
    fn ordinary_additions_reject_missing_parents_before_mutation() {
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let patch = PatchSet {
            added_instances: vec![
                PatchAdd {
                    parent_id: root,
                    instance: InstanceSnapshot::new().name("Valid"),
                },
                PatchAdd {
                    parent_id: Ref::none(),
                    instance: InstanceSnapshot::new().name("Invalid"),
                },
            ],
            ..PatchSet::new()
        };
        assert!(try_apply_patch_set(&mut tree, patch).is_err());
        assert!(tree.root().children().is_empty());
    }

    #[test]
    fn update_existing() {
        let _ = env_logger::try_init();

        let mut tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("OldClassName")
                .name("OldName")
                .property("Foo", 7i32)
                .property("Bar", 3i32)
                .property("Unchanged", -5i32),
        );

        let root_id = tree.get_root_id();

        let patch = PatchUpdate {
            id: root_id,
            changed_name: Some("Foo".to_owned()),
            changed_class_name: Some(ustr("NewClassName")),
            changed_parent: None,
            changed_properties: UstrMap::from_iter([
                // The value of Foo has changed
                (ustr("Foo"), Some(Variant::Int32(8))),
                // Bar has been deleted
                (ustr("Bar"), None),
                // Baz has been added
                (ustr("Baz"), Some(Variant::Int32(10))),
            ]),
            changed_metadata: None,
        };

        let patch_set = PatchSet {
            updated_instances: vec![patch],
            ..Default::default()
        };

        apply_patch_set(&mut tree, patch_set);

        let expected_properties = UstrMap::from_iter([
            (ustr("Foo"), Variant::Int32(8)),
            (ustr("Baz"), Variant::Int32(10)),
            (ustr("Unchanged"), Variant::Int32(-5)),
        ]);

        let root_instance = tree.get_instance(root_id).unwrap();
        assert_eq!(root_instance.name(), "Foo");
        assert_eq!(root_instance.class_name(), "NewClassName");
        assert_eq!(root_instance.properties(), &expected_properties);
    }
}
