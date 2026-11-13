//! Decides which directories store their whole subtree in `init.meta.json`.
//!
//! Script-free subtrees have nothing that needs its own file, so writing them
//! into one metadata file avoids creating a directory for every descendant.

use std::collections::{HashMap, HashSet};

use rbx_dom_weak::{types::Ref, Ustr, WeakDom};

use crate::{
    snapshot::{InstigatingSource, RojoTree},
    snapshot_middleware::Middleware,
};

const SCRIPT_BASE_CLASS: &str = "LuaSourceContainer";

/// Returns whether instances of `class` hold Luau source, including any class
/// that inherits from `LuaSourceContainer`.
pub fn is_script_class(class: &str) -> bool {
    let database = rbx_reflection_database::get().unwrap();
    let mut current = class;
    loop {
        if current == SCRIPT_BASE_CLASS {
            return true;
        }
        match database
            .classes
            .get(current)
            .and_then(|class| class.superclass)
        {
            Some(superclass) => current = superclass,
            None => return false,
        }
    }
}

/// Returns every instance in `dom` that is a script or has a script
/// descendant. Computing this once keeps the inlining decision linear for the
/// whole tree instead of rescanning each directory's subtree.
pub fn script_containers(dom: &WeakDom) -> HashSet<Ref> {
    let mut ordered = vec![dom.root_ref()];
    let mut index = 0;
    while index < ordered.len() {
        let instance = dom.get_by_ref(ordered[index]).unwrap();
        ordered.extend(instance.children().iter().copied());
        index += 1;
    }

    let mut classes: HashMap<Ustr, bool> = HashMap::new();
    let mut containers = HashSet::new();
    // Children come after their parents, so walking backwards settles every
    // child before the parent that depends on it.
    for referent in ordered.into_iter().rev() {
        let instance = dom.get_by_ref(referent).unwrap();
        let is_script = *classes
            .entry(instance.class)
            .or_insert_with(|| is_script_class(&instance.class));
        if is_script
            || instance
                .children()
                .iter()
                .any(|child| containers.contains(child))
        {
            containers.insert(referent);
        }
    }
    containers
}

/// Returns every instance in `tree` whose subtree is on disk in an outdated
/// layout: some file-backed instance in it belongs inside its directory
/// parent's `init.meta.json`, because that parent's subtree has no scripts.
/// Syncback skips subtrees whose content is unchanged, so without this an
/// existing project would never move into the inline layout.
pub fn needs_relayout(tree: &RojoTree) -> HashSet<Ref> {
    let containers = script_containers(tree.inner());
    let mut relayout = HashSet::new();
    let mut pending = vec![tree.get_root_id()];
    while let Some(referent) = pending.pop() {
        let Some(instance) = tree.get_instance(referent) else {
            continue;
        };
        pending.extend(instance.children().iter().copied());

        let parent = instance.parent();
        let misplaced = matches!(
            instance.metadata().instigating_source,
            Some(InstigatingSource::Path(_))
        ) && !containers.contains(&parent)
            && tree.get_metadata(parent).is_some_and(|metadata| {
                metadata.middleware == Some(Middleware::Dir)
                    && matches!(
                        metadata.instigating_source,
                        Some(InstigatingSource::Path(_))
                    )
            });
        if misplaced {
            // Every ancestor's subtree includes the misplaced instance.
            let mut current = parent;
            while relayout.insert(current) {
                match tree.get_instance(current) {
                    Some(ancestor) if ancestor.parent().is_some() => current = ancestor.parent(),
                    _ => break,
                }
            }
        }
    }
    relayout
}

/// Returns whether `id` is a script or has a script descendant.
pub fn contains_script(dom: &WeakDom, id: Ref) -> bool {
    let mut pending = vec![id];
    while let Some(referent) = pending.pop() {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        if is_script_class(&instance.class) {
            return true;
        }
        pending.extend(instance.children().iter().copied());
    }
    false
}

#[cfg(test)]
mod test {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn script_classes_are_detected_through_inheritance() {
        for (class, expected) in [
            ("Script", true),
            ("LocalScript", true),
            ("ModuleScript", true),
            ("LuaSourceContainer", true),
            ("Folder", false),
            ("Part", false),
            ("StringValue", false),
            ("NotARealClass", false),
        ] {
            assert_eq!(is_script_class(class), expected, "{class}");
        }
    }

    #[test]
    fn containers_match_per_instance_scans() {
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let root = dom.root_ref();
        let plain = dom.insert(root, InstanceBuilder::new("Folder"));
        let plain_leaf = dom.insert(plain, InstanceBuilder::new("Part"));
        let mixed = dom.insert(root, InstanceBuilder::new("Model"));
        let deep = dom.insert(mixed, InstanceBuilder::new("Folder"));
        let script = dom.insert(deep, InstanceBuilder::new("LocalScript"));
        let script_child = dom.insert(script, InstanceBuilder::new("Part"));

        let containers = script_containers(&dom);
        for referent in [root, plain, plain_leaf, mixed, deep, script, script_child] {
            assert_eq!(
                containers.contains(&referent),
                contains_script(&dom, referent),
                "{}",
                dom.get_by_ref(referent).unwrap().class
            );
        }
        assert_eq!(
            containers,
            HashSet::from([root, mixed, deep, script]),
            "only scripts and their ancestors contain scripts"
        );
    }
}
