//! Defines the algorithm for computing a roughly-minimal patch set given an
//! existing instance tree and an instance snapshot.

use std::{collections::HashMap, mem::take};

use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, HashMapExt as _, UstrMap, UstrSet,
};

use crate::{variant_eq::variant_eq, RojoRef, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX};

use super::{
    patch::{PatchAdd, PatchSet, PatchUpdate},
    InstanceSnapshot, InstanceWithMeta, RojoTree,
};

#[profiling::function]
pub fn compute_patch_set(snapshot: Option<InstanceSnapshot>, tree: &RojoTree, id: Ref) -> PatchSet {
    compute_patch_set_with(snapshot, tree, id, false, false)
}

/// Like [`compute_patch_set`], for a snapshot from `snapshot_shallow_from_vfs`.
/// Children with their own source files were left out of that read, so they're
/// kept rather than treated as removed.
pub fn compute_shallow_patch_set(
    snapshot: Option<InstanceSnapshot>,
    tree: &RojoTree,
    id: Ref,
) -> PatchSet {
    compute_patch_set_with(snapshot, tree, id, true, false)
}

/// Reconciles a saved owner exactly so approximate float equality cannot hide
/// canonical numeric corrections. Independently sourced children remain untouched.
pub(crate) fn compute_saved_patch_set(
    snapshot: Option<InstanceSnapshot>,
    tree: &RojoTree,
    id: Ref,
) -> PatchSet {
    compute_patch_set_with(snapshot, tree, id, true, true)
}

fn compute_patch_set_with(
    snapshot: Option<InstanceSnapshot>,
    tree: &RojoTree,
    id: Ref,
    keep_sourced_children: bool,
    exact_values: bool,
) -> PatchSet {
    let mut patch_set = PatchSet::new();

    if let Some(snapshot) = snapshot {
        let mut context = ComputePatchContext {
            exact_values,
            keep_sourced_children,
            ..Default::default()
        };

        compute_patch_set_internal(&mut context, snapshot, tree, id, &mut patch_set);

        // Rewrite Ref properties to refer to instance IDs instead of snapshot IDs
        // for all of the IDs that we know about so far.
        rewrite_refs_in_updates(&context, &mut patch_set.updated_instances);
        rewrite_refs_in_additions(&context, &mut patch_set.added_instances);
        drop_unchanged_refs(tree, &mut patch_set.updated_instances);
    } else if id != tree.get_root_id() {
        patch_set.removed_instances.push(id);
    }

    patch_set
}

#[derive(Default)]
struct ComputePatchContext {
    snapshot_id_to_instance_id: HashMap<Ref, Ref>,
    keep_sourced_children: bool,
    exact_values: bool,
}

fn rewrite_refs_in_updates(context: &ComputePatchContext, updates: &mut [PatchUpdate]) {
    for update in updates {
        for property_value in update.changed_properties.values_mut() {
            if let Some(Variant::Ref(referent)) = property_value {
                if let Some(&instance_ref) = context.snapshot_id_to_instance_id.get(referent) {
                    *property_value = Some(Variant::Ref(instance_ref));
                }
            }
        }
    }
}

/// A snapshot's references hold snapshot ids, so they only compare equal to the
/// tree once rewritten. Drops the ones that turned out unchanged, and updates
/// left with nothing to change, so rereading a file with references is a no-op.
fn drop_unchanged_refs(tree: &RojoTree, updates: &mut Vec<PatchUpdate>) {
    updates.retain_mut(|update| {
        if let Some(instance) = tree.get_instance(update.id) {
            update.changed_properties.retain(|name, value| match value {
                Some(Variant::Ref(referent)) => {
                    instance.properties().get(name) != Some(&Variant::Ref(*referent))
                }
                _ => true,
            });
        }
        update.changed_name.is_some()
            || update.changed_class_name.is_some()
            || update.changed_parent.is_some()
            || update.changed_metadata.is_some()
            || !update.changed_properties.is_empty()
    });
}

fn rewrite_refs_in_additions(context: &ComputePatchContext, additions: &mut [PatchAdd]) {
    for addition in additions {
        rewrite_refs_in_snapshot(context, &mut addition.instance);
    }
}

fn rewrite_refs_in_snapshot(context: &ComputePatchContext, snapshot: &mut InstanceSnapshot) {
    for property_value in snapshot.properties.values_mut() {
        if let Variant::Ref(referent) = property_value {
            if let Some(&instance_referent) = context.snapshot_id_to_instance_id.get(referent) {
                *property_value = Variant::Ref(instance_referent);
            }
        }
    }

    for child in &mut snapshot.children {
        rewrite_refs_in_snapshot(context, child);
    }
}

fn compute_patch_set_internal(
    context: &mut ComputePatchContext,
    mut snapshot: InstanceSnapshot,
    tree: &RojoTree,
    id: Ref,
    patch_set: &mut PatchSet,
) {
    if snapshot.snapshot_id.is_some() {
        context
            .snapshot_id_to_instance_id
            .insert(snapshot.snapshot_id, id);
    }

    let instance = tree
        .get_instance(id)
        .expect("Instance did not exist in tree");

    compute_property_patches(
        &mut snapshot,
        &instance,
        patch_set,
        tree,
        context.exact_values,
    );
    compute_children_patches(context, &mut snapshot, tree, id, patch_set);
}

fn compute_property_patches(
    snapshot: &mut InstanceSnapshot,
    instance: &InstanceWithMeta,
    patch_set: &mut PatchSet,
    tree: &RojoTree,
    exact_values: bool,
) {
    let mut visited_properties = UstrSet::default();
    let mut changed_properties = UstrMap::new();

    let attribute_ref_properties = compute_ref_properties(snapshot, tree);

    let changed_name = if snapshot.name == instance.name() {
        None
    } else {
        Some(take(&mut snapshot.name).into_owned())
    };

    let changed_class_name = if snapshot.class_name == instance.class_name() {
        None
    } else {
        Some(take(&mut snapshot.class_name))
    };

    // Applying a patch gives an instance the ID in its `Rojo_Id` attribute, so a
    // fresh snapshot only matches the tree once it carries that ID as well.
    if let Some(specified_id) = attribute_specified_id(snapshot) {
        snapshot.metadata.specified_id = Some(specified_id);
    }
    let changed_metadata = if &snapshot.metadata == instance.metadata() {
        None
    } else {
        Some(take(&mut snapshot.metadata))
    };

    for (name, snapshot_value) in take(&mut snapshot.properties) {
        visited_properties.insert(name);

        match instance.properties().get(&name) {
            Some(instance_value) => {
                let equal = variant_eq(&snapshot_value, instance_value)
                    && (!exact_values
                        || serde_json::to_value(&snapshot_value).expect("Variants must serialize")
                            == serde_json::to_value(instance_value)
                                .expect("Variants must serialize"));
                if !equal {
                    changed_properties.insert(name, Some(snapshot_value));
                }
            }
            None => {
                changed_properties.insert(name, Some(snapshot_value));
            }
        }
    }

    for name in instance.properties().keys() {
        if visited_properties.contains(name) {
            continue;
        }

        changed_properties.insert(*name, None);
    }

    for (name, ref_value) in attribute_ref_properties {
        match (&ref_value, instance.properties().get(&name)) {
            (Some(referent), Some(instance_value)) => {
                if referent != instance_value {
                    changed_properties.insert(name, ref_value);
                } else {
                    changed_properties.remove(&name);
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                changed_properties.insert(name, ref_value);
            }
            (None, None) => {
                changed_properties.remove(&name);
            }
        }
    }

    // !!!!!!!!!! UGLY HACK !!!!!!!!!!
    //
    // See RojoTree::insert_instance. Adjust that code also if you are touching this.
    let actual_class = changed_class_name.unwrap_or(instance.class_name());
    match actual_class.as_str() {
        "Model" | "Actor" | "Tool" | "HopperBin" | "Flag" | "WorldModel" | "Workspace"
        | "Status" => {
            let migration_prop = ustr("NeedsPivotMigration");
            // We want to just ignore this if it's being removed by a patch.
            // Normally this would not matter because serving != building but
            // if we start syncing models using SerializationService
            // (or GetObjects) it will affect how Studio deserializes things.
            if !instance.properties().contains_key(&migration_prop) {
                changed_properties
                    .entry(migration_prop)
                    .or_insert(Some(Variant::Bool(false)));
            }
            match changed_properties.get(&migration_prop) {
                // A snapshot that agrees with the tree, like a model file that
                // stores the property, must not produce an update. Studio can't
                // set it, so it would replace the whole model.
                None | Some(Some(Variant::Bool(_))) => {}
                Some(None) => {
                    changed_properties.remove(&migration_prop);
                }
                _ => {
                    changed_properties.insert(migration_prop, Some(Variant::Bool(false)));
                }
            }
        }
        _ => {}
    };

    if changed_properties.is_empty()
        && changed_name.is_none()
        && changed_class_name.is_none()
        && changed_metadata.is_none()
    {
        return;
    }

    patch_set.updated_instances.push(PatchUpdate {
        id: instance.id(),
        changed_name,
        changed_class_name,
        changed_parent: None,
        changed_properties,
        changed_metadata,
    });
}

fn compute_children_patches(
    context: &mut ComputePatchContext,
    snapshot: &mut InstanceSnapshot,
    tree: &RojoTree,
    id: Ref,
    patch_set: &mut PatchSet,
) {
    let instance = tree
        .get_instance(id)
        .expect("Instance did not exist in tree");

    let instance_children = instance.children();

    let mut paired_instances = vec![false; instance_children.len()];

    for snapshot_child in take(&mut snapshot.children) {
        let matching_instance =
            instance_children
                .iter()
                .enumerate()
                .find(|(instance_index, instance_child_id)| {
                    if paired_instances[*instance_index] {
                        return false;
                    }

                    let instance_child = tree
                        .get_instance(**instance_child_id)
                        .expect("Instance did not exist in tree");

                    // A shallow read never contains children with their own files,
                    // so one of those must not stand in for a same-named inline child.
                    if context.keep_sourced_children
                        && instance_child.metadata().instigating_source.is_some()
                    {
                        return false;
                    }

                    if snapshot_child.name == instance_child.name()
                        && snapshot_child.class_name == instance_child.class_name()
                    {
                        paired_instances[*instance_index] = true;
                        return true;
                    }

                    false
                });

        match matching_instance {
            Some((_, instance_child_id)) => {
                compute_patch_set_internal(
                    context,
                    snapshot_child,
                    tree,
                    *instance_child_id,
                    patch_set,
                );
            }
            None => {
                patch_set.added_instances.push(PatchAdd {
                    parent_id: id,
                    instance: snapshot_child,
                });
            }
        }
    }

    for (instance_index, instance_child_id) in instance_children.iter().enumerate() {
        if paired_instances[instance_index] {
            continue;
        }

        if context.keep_sourced_children
            && tree
                .get_metadata(*instance_child_id)
                .is_some_and(|metadata| metadata.instigating_source.is_some())
        {
            continue;
        }

        patch_set.removed_instances.push(*instance_child_id);
    }
}

/// Returns the ID a snapshot's `Rojo_Id` attribute gives it, read the same way
/// patch application reads it.
fn attribute_specified_id(snapshot: &InstanceSnapshot) -> Option<RojoRef> {
    let Some(Variant::Attributes(attributes)) = snapshot.properties.get(&ustr("Attributes")) else {
        return None;
    };
    match attributes.get(REF_ID_ATTRIBUTE_NAME)? {
        Variant::String(id) => Some(RojoRef::new(id.clone())),
        Variant::BinaryString(id) => std::str::from_utf8(id.as_ref())
            .ok()
            .map(|id| RojoRef::new(id.to_owned())),
        _ => None,
    }
}

fn compute_ref_properties(
    snapshot: &InstanceSnapshot,
    tree: &RojoTree,
) -> UstrMap<Option<Variant>> {
    let mut map = UstrMap::new();
    let attributes = match snapshot.properties.get(&ustr("Attributes")) {
        Some(Variant::Attributes(attrs)) => attrs,
        _ => return map,
    };

    for (attr_name, attr_value) in attributes.iter() {
        let prop_name = match attr_name.strip_prefix(REF_POINTER_ATTRIBUTE_PREFIX) {
            Some(str) => str,
            None => continue,
        };
        let rojo_ref = match attr_value {
            Variant::String(str) => RojoRef::new(str.clone()),
            Variant::BinaryString(bytes) => {
                if let Ok(str) = std::str::from_utf8(bytes.as_ref()) {
                    RojoRef::new(str.to_string())
                } else {
                    log::warn!(
                        "IDs specified by referent property attributes must be valid UTF-8 strings"
                    );
                    continue;
                }
            }
            _ => {
                log::warn!(
                    "Attribute {attr_name} is of type {:?} when it was \
                expected to be a String",
                    attr_value.ty()
                );
                continue;
            }
        };
        if let Some(target_id) = tree.get_specified_id(&rojo_ref) {
            map.insert(ustr(prop_name), Some(Variant::Ref(target_id)));
        } else {
            map.insert(ustr(prop_name), None);
        }
    }

    map
}

#[cfg(test)]
mod test {
    use super::*;

    use std::borrow::Cow;

    use rbx_dom_weak::types::Attributes;

    use crate::snapshot::apply_patch_set;

    /// An unchanged instance whose ID comes from its `Rojo_Id` attribute must not
    /// look changed, or two-way sync would treat every write to it as a conflict.
    #[test]
    fn attribute_ids_do_not_report_metadata_changes() {
        for id in [
            Variant::String("target".into()),
            Variant::BinaryString(b"target".to_vec().into()),
        ] {
            let mut attributes = Attributes::new();
            attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), id);
            let child = InstanceSnapshot::new()
                .name("Target")
                .class_name("Part")
                .properties(UstrMap::from_iter([(
                    ustr("Attributes"),
                    Variant::Attributes(attributes),
                )]));
            let root = InstanceSnapshot::new().name("Root").children(vec![child]);
            let mut tree = RojoTree::new(InstanceSnapshot::new().name("Root"));
            let root_id = tree.get_root_id();
            let patch = compute_patch_set(Some(root.clone()), &tree, root_id);
            apply_patch_set(&mut tree, patch);
            let target = tree.root().children()[0];
            assert_eq!(
                tree.get_specified_id(&RojoRef::new("target".into())),
                Some(target)
            );
            assert_eq!(
                compute_patch_set(Some(root), &tree, root_id),
                PatchSet::new()
            );
        }
    }

    /// This test makes sure that rewriting refs in instance update patches to
    /// instances that already exists works. We should be able to correlate the
    /// snapshot ID and instance ID during patch computation and replace the
    /// value before returning from compute_patch_set.
    #[test]
    fn rewrite_ref_existing_instance_update() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("foo").class_name("foo"));

        let root_id = tree.get_root_id();

        // This snapshot should be identical to the existing tree except for the
        // addition of a prop named Self, which is a self-referential Ref.
        let snapshot_id = Ref::new();
        let snapshot = InstanceSnapshot {
            snapshot_id,
            properties: UstrMap::from_iter([(ustr("Self"), Variant::Ref(snapshot_id))]),

            metadata: Default::default(),
            name: Cow::Borrowed("foo"),
            class_name: ustr("foo"),
            children: Vec::new(),
        };

        let patch_set = compute_patch_set(Some(snapshot), &tree, root_id);

        let expected_patch_set = PatchSet {
            updated_instances: vec![PatchUpdate {
                id: root_id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    ustr("Self"),
                    Some(Variant::Ref(root_id)),
                )]),
                changed_metadata: None,
            }],
            added_instances: Vec::new(),
            removed_instances: Vec::new(),
        };

        assert_eq!(patch_set, expected_patch_set);
    }

    /// A file that still holds the references the tree has, like a model file
    /// read again after a write, must not produce updates.
    #[test]
    fn unchanged_refs_are_not_updates() {
        let snapshot = || {
            let target = Ref::new();
            InstanceSnapshot::new()
                .name("foo")
                .class_name("foo")
                .children(vec![
                    InstanceSnapshot::new()
                        .name("Weld")
                        .class_name("Weld")
                        .properties(UstrMap::from_iter([(ustr("Part0"), Variant::Ref(target))])),
                    InstanceSnapshot::new()
                        .name("Part")
                        .class_name("Part")
                        .snapshot_id(target),
                ])
        };
        let mut tree = RojoTree::new(InstanceSnapshot::new().name("foo").class_name("foo"));
        let root_id = tree.get_root_id();
        let initial = compute_patch_set(Some(snapshot()), &tree, root_id);
        apply_patch_set(&mut tree, initial);

        assert_eq!(
            compute_patch_set(Some(snapshot()), &tree, root_id),
            PatchSet::new()
        );
    }

    /// The same as rewrite_ref_existing_instance_update, except that the
    /// property is added in a new instance instead of modifying an existing
    /// one.
    #[test]
    fn rewrite_ref_existing_instance_addition() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("foo").class_name("foo"));

        let root_id = tree.get_root_id();

        // This patch describes the existing instance with a new child added.
        let snapshot_id = Ref::new();
        let snapshot = InstanceSnapshot {
            snapshot_id,
            children: vec![InstanceSnapshot {
                properties: UstrMap::from_iter([(ustr("Self"), Variant::Ref(snapshot_id))]),

                snapshot_id: Ref::none(),
                metadata: Default::default(),
                name: Cow::Borrowed("child"),
                class_name: ustr("child"),
                children: Vec::new(),
            }],

            metadata: Default::default(),
            properties: UstrMap::new(),
            name: Cow::Borrowed("foo"),
            class_name: ustr("foo"),
        };

        let patch_set = compute_patch_set(Some(snapshot), &tree, root_id);

        let expected_patch_set = PatchSet {
            added_instances: vec![PatchAdd {
                parent_id: root_id,
                instance: InstanceSnapshot {
                    snapshot_id: Ref::none(),
                    metadata: Default::default(),
                    properties: UstrMap::from_iter([(ustr("Self"), Variant::Ref(root_id))]),
                    name: Cow::Borrowed("child"),
                    class_name: ustr("child"),
                    children: Vec::new(),
                },
            }],
            updated_instances: Vec::new(),
            removed_instances: Vec::new(),
        };

        assert_eq!(patch_set, expected_patch_set);
    }
}
