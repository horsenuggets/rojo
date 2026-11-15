//! Hashing utilities for a WeakDom.
mod variant;

pub use variant::*;

use blake3::{Hash, Hasher};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, Ustr, WeakDom,
};
use std::collections::HashMap;

use crate::{variant_eq::variant_eq, Project};

use super::{descendants, filter_json_properties_preallocated, filter_properties_preallocated};

#[derive(Clone, Copy)]
enum PropertyHashMode {
    Raw,
    Json,
    Binary,
}

/// Returns a map of every `Ref` in the `WeakDom` to a hashed version of the
/// `Instance` it points to, including the properties and descendants of the
/// `Instance`.
///
/// The hashes **do** include the descendants of the Instances in them,
/// so they should only be used for comparing subtrees directly.
pub fn hash_tree(project: &Project, dom: &WeakDom, root_ref: Ref) -> HashMap<Ref, Hash> {
    hash_tree_with_mode(project, dom, root_ref, PropertyHashMode::Raw)
}

pub fn hash_json_tree(project: &Project, dom: &WeakDom, root_ref: Ref) -> HashMap<Ref, Hash> {
    hash_tree_with_mode(project, dom, root_ref, PropertyHashMode::Json)
}

pub fn hash_binary_tree(project: &Project, dom: &WeakDom, root_ref: Ref) -> HashMap<Ref, Hash> {
    hash_tree_with_mode(project, dom, root_ref, PropertyHashMode::Binary)
}

fn hash_tree_with_mode(
    project: &Project,
    dom: &WeakDom,
    root_ref: Ref,
    mode: PropertyHashMode,
) -> HashMap<Ref, Hash> {
    let mut order = descendants(dom, root_ref);
    let mut map: HashMap<Ref, Hash> = HashMap::with_capacity(order.len());

    let mut prop_list = Vec::with_capacity(2);
    let mut child_hashes = Vec::new();

    while let Some(referent) = order.pop() {
        let inst = dom.get_by_ref(referent).unwrap();
        let mut hasher = hash_inst_filtered(project, inst, &mut prop_list, mode);
        add_children(inst, &map, &mut child_hashes, &mut hasher);

        map.insert(referent, hasher.finalize());
    }

    map
}

/// Hashes a single Instance from the provided WeakDom, if it exists.
///
/// This function filters properties using user-provided syncing rules from
/// the passed project.
#[inline]
pub fn hash_json_instance(project: &Project, dom: &WeakDom, referent: Ref) -> Option<Hash> {
    let mut prop_list = Vec::with_capacity(2);
    let inst = dom.get_by_ref(referent)?;

    Some(hash_inst_filtered(project, inst, &mut prop_list, PropertyHashMode::Json).finalize())
}

/// Adds the hashes of children for an Instance to the provided Hasher.
fn add_children(
    inst: &Instance,
    map: &HashMap<Ref, Hash>,
    child_hashes: &mut Vec<[u8; 32]>,
    hasher: &mut Hasher,
) {
    for child_ref in inst.children() {
        if let Some(hash) = map.get(child_ref) {
            child_hashes.push(*hash.as_bytes())
        } else {
            panic!("Invariant violated: child not hashed before parent")
        }
    }
    child_hashes.sort_unstable();

    for hash in child_hashes.drain(..) {
        hasher.update(&hash);
    }
}

/// Performs hashing on an Instance using a filtered property list.
/// Does not include the hashes of any children.
fn hash_inst_filtered<'inst>(
    project: &Project,
    inst: &'inst Instance,
    prop_list: &mut Vec<(Ustr, &'inst Variant)>,
    mode: PropertyHashMode,
) -> Hasher {
    match mode {
        PropertyHashMode::Raw | PropertyHashMode::Binary => {
            filter_properties_preallocated(project, inst, prop_list)
        }
        PropertyHashMode::Json => filter_json_properties_preallocated(project, inst, prop_list),
    }

    hash_inst_prefilled(inst, prop_list, mode)
}

/// Performs hashing on an Instance using a pre-filled list of properties.
/// It is assumed the property list is **not** sorted, so it is sorted in-line.
fn hash_inst_prefilled<'inst>(
    inst: &'inst Instance,
    prop_list: &mut Vec<(Ustr, &'inst Variant)>,
    mode: PropertyHashMode,
) -> Hasher {
    let hash_value = match mode {
        PropertyHashMode::Raw => hash_exact_variant,
        PropertyHashMode::Json => hash_json_variant,
        PropertyHashMode::Binary => hash_variant,
    };
    let mut hasher = Hasher::new();
    hasher.update(inst.name.as_bytes());
    hasher.update(inst.class.as_bytes());

    prop_list.sort_unstable_by_key(|(name, _)| *name);

    let descriptor = rbx_reflection_database::get()
        .unwrap()
        .classes
        .get(inst.class.as_str());

    if let Some(descriptor) = descriptor {
        for (name, value) in prop_list.drain(..) {
            hasher.update(name.as_bytes());
            if let Some(default) = descriptor.default_properties.get(name.as_str()) {
                if !variant_eq(default, value) {
                    hash_value(&mut hasher, value)
                }
            } else {
                hash_value(&mut hasher, value)
            }
        }
    } else {
        for (name, value) in prop_list.drain(..) {
            hasher.update(name.as_bytes());
            hash_value(&mut hasher, value)
        }
    }

    hasher
}
