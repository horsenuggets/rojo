use std::borrow::Cow;

use insta::assert_yaml_snapshot;

use rbx_dom_weak::{types::Ref, ustr, UstrMap};
use rojo_insta_ext::RedactionMap;

use crate::snapshot::{compute_patch_set, compute_shallow_patch_set, InstanceSnapshot, RojoTree};

#[test]
fn set_name_and_class_name() {
    let mut redactions = RedactionMap::default();

    let tree = empty_tree();
    redactions.intern(tree.get_root_id());

    let snapshot = InstanceSnapshot {
        snapshot_id: Ref::none(),
        metadata: Default::default(),
        name: Cow::Borrowed("Some Folder"),
        class_name: ustr("Folder"),
        properties: Default::default(),
        children: Vec::new(),
    };

    let patch_set = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
    let patch_value = redactions.redacted_yaml(patch_set);

    assert_yaml_snapshot!(patch_value);
}

#[test]
fn set_property() {
    let mut redactions = RedactionMap::default();

    let tree = empty_tree();
    redactions.intern(tree.get_root_id());

    let snapshot = InstanceSnapshot {
        snapshot_id: Ref::none(),
        metadata: Default::default(),
        name: Cow::Borrowed("ROOT"),
        class_name: ustr("ROOT"),
        properties: UstrMap::from_iter([(ustr("PropertyName"), "Hello, world!".into())]),
        children: Vec::new(),
    };

    let patch_set = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
    let patch_value = redactions.redacted_yaml(patch_set);

    assert_yaml_snapshot!(patch_value);
}

#[test]
fn remove_property() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    redactions.intern(tree.get_root_id());

    {
        let root_id = tree.get_root_id();
        let mut root_instance = tree.get_instance_mut(root_id).unwrap();
        root_instance
            .properties_mut()
            .insert(ustr("Foo"), "This should be removed by the patch.".into());
    }

    let snapshot = InstanceSnapshot {
        snapshot_id: Ref::none(),
        metadata: Default::default(),
        name: Cow::Borrowed("ROOT"),
        class_name: ustr("ROOT"),
        properties: Default::default(),
        children: Vec::new(),
    };

    let patch_set = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
    let patch_value = redactions.redacted_yaml(patch_set);

    assert_yaml_snapshot!(patch_value);
}

#[test]
fn add_child() {
    let mut redactions = RedactionMap::default();

    let tree = empty_tree();
    redactions.intern(tree.get_root_id());

    let snapshot = InstanceSnapshot {
        snapshot_id: Ref::none(),
        metadata: Default::default(),
        name: Cow::Borrowed("ROOT"),
        class_name: ustr("ROOT"),
        properties: Default::default(),
        children: vec![InstanceSnapshot {
            snapshot_id: Ref::none(),
            metadata: Default::default(),
            name: Cow::Borrowed("New"),
            class_name: ustr("Folder"),
            properties: Default::default(),
            children: Vec::new(),
        }],
    };

    let patch_set = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
    let patch_value = redactions.redacted_yaml(patch_set);

    assert_yaml_snapshot!(patch_value);
}

#[test]
fn remove_child() {
    let mut redactions = RedactionMap::default();

    let mut tree = empty_tree();
    redactions.intern(tree.get_root_id());

    {
        let root_id = tree.get_root_id();
        let new_id = tree.insert_instance(
            root_id,
            InstanceSnapshot::new().name("Should not appear in snapshot"),
        );

        redactions.intern(new_id);
    }

    let snapshot = InstanceSnapshot {
        snapshot_id: Ref::none(),
        metadata: Default::default(),
        name: Cow::Borrowed("ROOT"),
        class_name: ustr("ROOT"),
        properties: Default::default(),
        children: Vec::new(),
    };

    let patch_set = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
    let patch_value = redactions.redacted_yaml(patch_set);

    assert_yaml_snapshot!(patch_value);
}

/// Walks every combination of whether a child has its own source file, whether
/// the new snapshot still contains it, and whether the comparison is shallow.
/// Only a shallow comparison may keep a missing child, and only one that has
/// its own source, since a shallow read never includes those.
#[test]
fn shallow_patches_keep_only_children_with_their_own_source() {
    for bits in 0..8u8 {
        let sourced = bits & 1 != 0;
        let in_snapshot = bits & 2 != 0;
        let shallow = bits & 4 != 0;

        let mut child = InstanceSnapshot::new().name("Child").class_name("Folder");
        if sourced {
            child.metadata = child
                .metadata
                .instigating_source(std::path::PathBuf::from("/root/Child"));
        }
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .name("ROOT")
                .class_name("Folder")
                .children(vec![child.clone()]),
        );
        let child_id = tree.root().children()[0];

        let mut snapshot = InstanceSnapshot::new()
            .name("ROOT")
            .class_name("Folder")
            .property(ustr("Changed"), "yes");
        if in_snapshot {
            snapshot.children.push(child);
        }

        let patch = if shallow {
            compute_shallow_patch_set(Some(snapshot), &tree, tree.get_root_id())
        } else {
            compute_patch_set(Some(snapshot), &tree, tree.get_root_id())
        };

        let removed = patch.removed_instances.contains(&child_id);
        let kept_by_shallow_read = shallow && sourced;
        let expect_removed = !(in_snapshot || kept_by_shallow_read);
        assert_eq!(removed, expect_removed, "bits {bits:03b}");
        // A shallow snapshot's child is an inline child, so it never pairs with
        // a file-backed sibling of the same name and is added instead.
        let expect_added = usize::from(in_snapshot && kept_by_shallow_read);
        assert_eq!(patch.added_instances.len(), expect_added, "bits {bits:03b}");
        // The root's own change is always found, shallow or not.
        assert!(
            patch
                .updated_instances
                .iter()
                .any(|update| update.id == tree.get_root_id()),
            "bits {bits:03b}"
        );
        assert!(
            patch
                .updated_instances
                .iter()
                .all(|update| update.id != child_id || !kept_by_shallow_read),
            "bits {bits:03b}"
        );
    }
}

fn empty_tree() -> RojoTree {
    RojoTree::new(InstanceSnapshot::new().name("ROOT").class_name("ROOT"))
}
