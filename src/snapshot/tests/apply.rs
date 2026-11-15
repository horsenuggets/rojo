use insta::assert_yaml_snapshot;

use rbx_dom_weak::{ustr, UstrMap};
use rojo_insta_ext::RedactionMap;

use crate::{
    snapshot::{apply_patch_set, InstanceSnapshot, PatchSet, PatchUpdate, RojoTree},
    tree_view::{intern_tree, view_tree},
};

#[test]
fn set_name_and_class_name() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: Some("Hello, world!".to_owned()),
            changed_class_name: Some(ustr("Folder")),
            changed_parent: None,
            changed_properties: Default::default(),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!(tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!(applied_patch_value);
}

#[test]
fn add_property() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: None,
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::from_iter([(ustr("Foo"), Some("Value of Foo".into()))]),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!(tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!(applied_patch_value);
}

#[test]
fn remove_property() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    intern_tree(&tree, &mut redactions);

    {
        let root_id = tree.get_root_id();
        let mut root_instance = tree.get_instance_mut(root_id).unwrap();

        root_instance
            .properties_mut()
            .insert(ustr("Foo"), "Should be removed".into());
    }

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!("remove_property_initial", tree_view);

    let patch_set = PatchSet {
        updated_instances: vec![PatchUpdate {
            id: tree.get_root_id(),
            changed_name: None,
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::from_iter([(ustr("Foo"), None)]),
            changed_metadata: None,
        }],
        ..Default::default()
    };

    let applied_patch_set = apply_patch_set(&mut tree, patch_set);

    let tree_view = view_tree(&tree, &mut redactions);
    assert_yaml_snapshot!("remove_property_after_patch", tree_view);

    let applied_patch_value = redactions.redacted_yaml(applied_patch_set);
    assert_yaml_snapshot!("remove_property_appied_patch", applied_patch_value);
}

#[test]
fn null_references_are_applied_and_sent_to_studio() {
    use rbx_dom_weak::types::{Ref, Variant};

    let mut tree = empty_tree();
    let id = tree.get_root_id();
    tree.get_instance_mut(id)
        .unwrap()
        .properties_mut()
        .insert(ustr("Target"), Variant::Ref(Ref::new()));
    let applied = apply_patch_set(
        &mut tree,
        PatchSet {
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    ustr("Target"),
                    Some(Variant::Ref(Ref::none())),
                )]),
                changed_metadata: None,
            }],
            ..PatchSet::new()
        },
    );
    assert_eq!(
        tree.get_instance(id).unwrap().properties()[&ustr("Target")],
        Variant::Ref(Ref::none())
    );
    assert_eq!(
        applied.updated[0].changed_properties[&ustr("Target")],
        Some(Variant::Ref(Ref::none()))
    );
}

fn move_update(id: rbx_dom_weak::types::Ref, parent: rbx_dom_weak::types::Ref) -> PatchUpdate {
    PatchUpdate {
        id,
        changed_parent: Some(parent),
        changed_name: None,
        changed_class_name: None,
        changed_properties: Default::default(),
        changed_metadata: None,
    }
}

#[test]
fn move_subtree_preserves_identity_and_references() {
    use crate::snapshot::InstanceMetadata;
    use crate::RojoRef;
    use rbx_dom_weak::types::Variant;

    let mut tree = empty_tree();
    let root = tree.get_root_id();
    let source = tree.insert_instance(root, InstanceSnapshot::new().name("Source"));
    let destination = tree.insert_instance(root, InstanceSnapshot::new().name("Destination"));
    let specified = RojoRef::new("Moved".into());
    let path = std::path::PathBuf::from("moved.lua");
    let moved = tree.insert_instance(
        source,
        InstanceSnapshot::new()
            .name("Moved")
            .metadata(InstanceMetadata {
                relevant_paths: vec![path.clone()],
                specified_id: Some(specified.clone()),
                ..Default::default()
            }),
    );
    let child = tree.insert_instance(moved, InstanceSnapshot::new().name("Child"));
    tree.get_instance_mut(child)
        .unwrap()
        .properties_mut()
        .insert(ustr("Target"), Variant::Ref(moved));
    tree.get_instance_mut(root)
        .unwrap()
        .properties_mut()
        .insert(ustr("Target"), Variant::Ref(child));
    let mut redactions = RedactionMap::default();
    intern_tree(&tree, &mut redactions);

    let applied = apply_patch_set(
        &mut tree,
        PatchSet {
            updated_instances: vec![move_update(moved, destination)],
            ..Default::default()
        },
    );
    assert_eq!(tree.get_instance(moved).unwrap().parent(), destination);
    assert_eq!(tree.get_instance(moved).unwrap().children(), &[child]);
    assert_eq!(tree.get_instance(child).unwrap().parent(), moved);
    assert!(tree.get_instance(source).unwrap().children().is_empty());
    assert_eq!(tree.get_ids_at_path(&path), &[moved]);
    assert_eq!(tree.get_specified_id(&specified), Some(moved));
    assert_eq!(
        tree.root().properties()[&ustr("Target")],
        Variant::Ref(child)
    );
    assert_eq!(
        tree.get_instance(child).unwrap().properties()[&ustr("Target")],
        Variant::Ref(moved)
    );
    assert!(applied.added.is_empty());
    assert!(applied.removed.is_empty());
    assert_eq!(applied.updated[0].changed_parent, Some(destination));
    assert_yaml_snapshot!(redactions.redacted_yaml(applied));
}

#[test]
fn final_graph_allows_ancestry_reversal_in_either_update_order() {
    for reverse in [false, true] {
        let mut tree = empty_tree();
        let root = tree.get_root_id();
        let a = tree.insert_instance(root, InstanceSnapshot::new());
        let b = tree.insert_instance(a, InstanceSnapshot::new());
        let mut updates = vec![move_update(a, b), move_update(b, root)];
        if reverse {
            updates.reverse();
        }
        apply_patch_set(
            &mut tree,
            PatchSet {
                updated_instances: updates,
                ..Default::default()
            },
        );
        assert_eq!(tree.get_instance(a).unwrap().parent(), b);
        assert_eq!(tree.get_instance(b).unwrap().parent(), root);
    }
}

#[test]
fn unchanged_parent_updates_preserve_sibling_order() {
    let mut tree = empty_tree();
    let root = tree.get_root_id();
    let folder = tree.insert_instance(root, InstanceSnapshot::new());
    let first = tree.insert_instance(folder, InstanceSnapshot::new());
    let second = tree.insert_instance(folder, InstanceSnapshot::new());
    apply_patch_set(
        &mut tree,
        PatchSet {
            updated_instances: vec![move_update(first, folder)],
            ..Default::default()
        },
    );
    assert_eq!(
        tree.get_instance(folder).unwrap().children(),
        &[first, second]
    );
}

#[test]
fn descendants_can_escape_removed_ancestors() {
    let mut tree = empty_tree();
    let root = tree.get_root_id();
    let removed = tree.insert_instance(root, InstanceSnapshot::new());
    let moved = tree.insert_instance(removed, InstanceSnapshot::new());
    let child = tree.insert_instance(moved, InstanceSnapshot::new());
    let deleted = tree.insert_instance(removed, InstanceSnapshot::new());
    apply_patch_set(
        &mut tree,
        PatchSet {
            removed_instances: vec![removed, deleted],
            updated_instances: vec![move_update(moved, root)],
            ..Default::default()
        },
    );
    assert!(tree.get_instance(removed).is_none());
    assert!(tree.get_instance(deleted).is_none());
    assert_eq!(tree.get_instance(moved).unwrap().children(), &[child]);
}

#[test]
fn all_small_parent_graphs_apply_if_and_only_if_acyclic() {
    use crate::snapshot::try_apply_patch_set;
    // Exhaust all parent assignments for three nodes, including unchanged parents,
    // self-parenting, and cycles spanning multiple simultaneous moves.
    for a_parent in 0..4 {
        for b_parent in 0..4 {
            for c_parent in 0..4 {
                let mut tree = empty_tree();
                let root = tree.get_root_id();
                let a = tree.insert_instance(root, InstanceSnapshot::new());
                let b = tree.insert_instance(a, InstanceSnapshot::new());
                let c = tree.insert_instance(b, InstanceSnapshot::new());
                let ids = [root, a, b, c];
                let parents = [0, a_parent, b_parent, c_parent];
                let valid = (1..4).all(|node| {
                    let mut current = node;
                    for _ in 0..3 {
                        current = parents[current];
                    }
                    current == 0
                });
                let patch = PatchSet {
                    updated_instances: (1..4)
                        .map(|node| move_update(ids[node], ids[parents[node]]))
                        .collect(),
                    ..Default::default()
                };
                assert_eq!(try_apply_patch_set(&mut tree, patch).is_ok(), valid);
                if valid {
                    for node in 1..4 {
                        assert_eq!(
                            tree.get_instance(ids[node]).unwrap().parent(),
                            ids[parents[node]]
                        );
                    }
                    assert_eq!(tree.descendants(root).count(), 4);
                } else {
                    assert_eq!(tree.root().children(), &[a]);
                    assert_eq!(tree.get_instance(a).unwrap().children(), &[b]);
                    assert_eq!(tree.get_instance(b).unwrap().children(), &[c]);
                }
            }
        }
    }
}

#[test]
fn invalid_final_graph_is_rejected_without_any_mutation() {
    use crate::snapshot::{try_apply_patch_set, PatchAdd};
    use rbx_dom_weak::types::Ref;
    for case in 0..10 {
        let mut tree = empty_tree();
        let root = tree.get_root_id();
        let a = tree.insert_instance(root, InstanceSnapshot::new().name("A"));
        let b = tree.insert_instance(a, InstanceSnapshot::new().name("B"));
        let mut patch = PatchSet::default();
        match case {
            0 => patch.updated_instances = vec![move_update(root, a)],
            1 => patch.updated_instances = vec![move_update(a, a)],
            2 => patch.updated_instances = vec![move_update(a, b)],
            3 => patch.updated_instances = vec![move_update(a, Ref::none())],
            4 => patch.updated_instances = vec![move_update(Ref::new(), root)],
            5 => patch.updated_instances = vec![move_update(a, root), move_update(a, b)],
            6 => {
                patch.removed_instances = vec![a];
                patch.updated_instances = vec![move_update(b, a)];
            }
            7 => patch.removed_instances = vec![root],
            8 => patch.removed_instances = vec![Ref::new()],
            9 => {
                patch.removed_instances = vec![a];
                patch.added_instances = vec![PatchAdd {
                    parent_id: b,
                    instance: InstanceSnapshot::new(),
                }];
            }
            _ => unreachable!(),
        }
        let mut rename = move_update(root, root);
        rename.changed_parent = None;
        rename.changed_name = Some("Must not apply".into());
        patch.updated_instances.insert(0, rename);
        assert!(
            try_apply_patch_set(&mut tree, patch).is_err(),
            "case {case}"
        );
        assert_eq!(tree.root().name(), "ROOT");
        assert_eq!(tree.root().children(), &[a]);
        assert_eq!(tree.get_instance(a).unwrap().children(), &[b]);
        assert_eq!(tree.get_instance(b).unwrap().parent(), a);
    }
}

fn empty_tree() -> RojoTree {
    RojoTree::new(InstanceSnapshot::new().name("ROOT").class_name("ROOT"))
}
