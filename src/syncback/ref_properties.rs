//! Implements iterating through an entire WeakDom and linking all Ref
//! properties using attributes.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use rbx_dom_weak::{
    types::{Attributes, Ref, UniqueId, Variant},
    ustr, Instance, Ustr, WeakDom,
};

use crate::syncback::snapshot::inst_path;
use crate::{multimap::MultiMap, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX};

pub struct RefLinks {
    /// A map of referents to each of their Ref properties.
    prop_links: MultiMap<Ref, RefLink>,
    /// A set of referents that need their ID rewritten. This includes
    /// Instances that have no existing ID.
    need_rewrite: HashSet<Ref>,
}

#[derive(PartialEq, Eq)]
struct RefLink {
    /// The name of a property
    name: Ustr,
    /// The value of the property.
    value: Ref,
}

/// Iterates through a WeakDom and collects referent properties.
///
/// They can be linked to a dom later using `link_referents`.
pub fn collect_referents(dom: &WeakDom) -> RefLinks {
    let mut ids = HashMap::new();
    let mut need_rewrite = HashSet::new();
    let mut links = MultiMap::new();

    // Note that this is back-in, front-out. This is important because
    // VecDeque::extend is the equivalent to using push_back.
    let mut queue = VecDeque::new();
    queue.push_back(dom.root_ref());
    while let Some(inst_ref) = queue.pop_front() {
        let pointer = dom.get_by_ref(inst_ref).unwrap();
        queue.extend(pointer.children().iter().copied());

        for (prop_name, prop_value) in &pointer.properties {
            let Variant::Ref(prop_value) = prop_value else {
                continue;
            };
            if prop_value.is_none() {
                continue;
            }

            let target = match dom.get_by_ref(*prop_value) {
                Some(inst) => inst,
                None => {
                    // Properties that are Some but point to nothing may as
                    // well be `nil`. Roblox and us never produce these values
                    // but syncback prunes trees without adjusting ref
                    // properties for performance reasons.
                    log::warn!(
                        "Property {}.{} will be `nil` on the disk because the actual value is not being included in syncback",
                        inst_path(dom, inst_ref),
                        prop_name
                    );
                    continue;
                }
            };

            links.insert(
                inst_ref,
                RefLink {
                    name: *prop_name,
                    value: *prop_value,
                },
            );

            // 1. Check if target has an ID
            if let Some(id) = get_existing_id(target) {
                // If it does, we need to check whether that ID is a duplicate
                if let Some(id_ref) = ids.get(id) {
                    // If the same ID points to a new Instance, rewrite it.
                    if id_ref != prop_value {
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!(
                                "{} needs an id rewritten because it has the same id as {}",
                                target.name,
                                dom.get_by_ref(*id_ref).unwrap().name
                            );
                        }
                        need_rewrite.insert(*prop_value);
                    }
                }
                ids.insert(id, *prop_value);
            } else {
                log::trace!("{} needs an id rewritten because it has no id but is referred to by {}.{prop_name}", target.name, pointer.name);
                // If it does not, it needs one.
                need_rewrite.insert(*prop_value);
            }
        }
    }

    RefLinks {
        need_rewrite,
        prop_links: links,
    }
}

/// Links Ref properties through attributes. An instance that needs an ID it
/// doesn't have takes its entry in `ids_on_disk` (see [`ids_on_disk`]) when it
/// has one, so the same place gets the same IDs every time it's synced back.
pub fn link_referents(
    links: RefLinks,
    dom: &mut WeakDom,
    ids_on_disk: &HashMap<Ref, String>,
) -> anyhow::Result<()> {
    write_id_attributes(&links, dom, ids_on_disk)?;

    let mut prop_list = Vec::new();

    for (inst_id, properties) in links.prop_links {
        for ref_link in properties {
            let prop_inst = match dom.get_by_ref(ref_link.value) {
                Some(inst) => inst,
                None => continue,
            };
            let id = get_existing_id(prop_inst)
                .expect("all Instances that are pointed to should have an ID");
            prop_list.push((ref_link.name, Variant::String(id.to_owned())));
        }
        let inst = match dom.get_by_ref_mut(inst_id) {
            Some(inst) => inst,
            None => continue,
        };

        let mut attributes: Attributes = match inst.properties.remove(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => Attributes::new(),
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        }
        .into_iter()
        .filter(|(name, _)| !name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX))
        .collect();

        for (prop_name, prop_value) in prop_list.drain(..) {
            attributes.insert(
                format!("{REF_POINTER_ATTRIBUTE_PREFIX}{prop_name}"),
                prop_value,
            );
        }

        inst.properties
            .insert("Attributes".into(), attributes.into());
    }

    Ok(())
}

fn write_id_attributes(
    links: &RefLinks,
    dom: &mut WeakDom,
    ids_on_disk: &HashMap<Ref, String>,
) -> anyhow::Result<()> {
    for referent in &links.need_rewrite {
        let inst = match dom.get_by_ref_mut(*referent) {
            Some(inst) => inst,
            None => continue,
        };
        // Studio hands out new UniqueIds every time it serializes, and some place
        // files have none, so an ID made from one can change from run to run.
        // The ID already on disk is kept instead whenever there is one.
        let id = match ids_on_disk.get(referent) {
            Some(id) => id.clone(),
            None => match inst.properties.get(&ustr("UniqueId")) {
                Some(Variant::UniqueId(id)) => *id,
                _ => UniqueId::now().unwrap(),
            }
            .to_string(),
        };

        let attributes = match inst.properties.get_mut(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => {
                inst.properties
                    .insert("Attributes".into(), Attributes::new().into());
                match inst.properties.get_mut(&ustr("Attributes")) {
                    Some(Variant::Attributes(attrs)) => attrs,
                    _ => unreachable!(),
                }
            }
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        };
        attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), Variant::String(id));
    }
    Ok(())
}

/// Finds the `Rojo_Id` each instance in `new` has on disk, for instances that
/// don't carry one themselves.
///
/// Instances are matched by path from the roots of `new` and `old`, and need the
/// same class. Siblings that share a name, which Roblox models are full of, are
/// matched in order, but only when both sides have the same number of them.
///
/// If siblings were reordered, two of them can trade IDs. Syncback relinks every
/// reference it writes in the same run, so those all still point at the right
/// instance. Only references outside what syncback writes, like ones in ignored
/// trees, could be affected, and those already broke on every run when IDs were
/// made up anew. An ID that some instance in `new` already has is never handed
/// out again.
pub fn ids_on_disk(new: &WeakDom, old: &WeakDom) -> HashMap<Ref, String> {
    let mut taken: HashSet<&str> = new.descendants().filter_map(get_existing_id).collect();

    let mut ids = HashMap::new();
    let mut pending = vec![(new.root_ref(), old.root_ref())];
    while let Some((new_ref, old_ref)) = pending.pop() {
        for (child, old_child) in matched_children(new, new_ref, old, old_ref) {
            if get_existing_id(child).is_none() {
                if let Some(id) = get_existing_id(old_child) {
                    if taken.insert(id) {
                        ids.insert(child.referent(), id.to_owned());
                    }
                }
            }
            pending.push((child.referent(), old_child.referent()));
        }
    }
    ids
}

/// Pairs the children of `new_parent` with the children of `old_parent` that have
/// the same name and class, matching siblings that share a name in order when
/// both sides have the same number of them.
fn matched_children<'a>(
    new: &'a WeakDom,
    new_parent: Ref,
    old: &'a WeakDom,
    old_parent: Ref,
) -> Vec<(&'a Instance, &'a Instance)> {
    let old_groups = children_by_name(old, old_parent);
    let mut pairs = Vec::new();
    for (name, new_group) in children_by_name(new, new_parent) {
        let Some(old_group) = old_groups.get(name) else {
            continue;
        };
        if old_group.len() != new_group.len() {
            continue;
        }
        for (new_child, old_child) in new_group.into_iter().zip(old_group) {
            if new_child.class == old_child.class {
                pairs.push((new_child, *old_child));
            }
        }
    }
    pairs
}

/// Groups the children of `parent` by name, keeping their order within each group.
fn children_by_name(dom: &WeakDom, parent: Ref) -> BTreeMap<&str, Vec<&Instance>> {
    let mut groups: BTreeMap<&str, Vec<&Instance>> = BTreeMap::new();
    for child_ref in dom.get_by_ref(parent).unwrap().children() {
        let child = dom.get_by_ref(*child_ref).unwrap();
        groups.entry(child.name.as_str()).or_default().push(child);
    }
    groups
}

/// Returns every ID that an instance in `dom` holds through its `Rojo_Id`
/// attribute. After `link_referents`, these are the IDs that Ref properties
/// in `dom` point at.
pub fn attribute_ids(dom: &WeakDom) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut pending = vec![dom.root_ref()];
    while let Some(referent) = pending.pop() {
        let inst = dom.get_by_ref(referent).unwrap();
        pending.extend(inst.children().iter().copied());
        if let Some(id) = get_existing_id(inst) {
            ids.insert(id.to_owned());
        }
    }
    ids
}

fn get_existing_id(inst: &Instance) -> Option<&str> {
    if let Variant::Attributes(attrs) = inst.properties.get(&ustr("Attributes"))? {
        let id = attrs.get(REF_ID_ATTRIBUTE_NAME)?;
        match id {
            Variant::String(str) => Some(str),
            Variant::BinaryString(bstr) => std::str::from_utf8(bstr.as_ref()).ok(),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use rbx_dom_weak::InstanceBuilder;

    use super::*;

    fn with_id(builder: InstanceBuilder, id: &str) -> InstanceBuilder {
        let mut attributes = Attributes::new();
        attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), Variant::String(id.into()));
        builder.with_property("Attributes", attributes)
    }

    fn named(dom: &WeakDom, name: &str) -> Vec<Ref> {
        dom.descendants()
            .filter(|inst| inst.name == name)
            .map(Instance::referent)
            .collect()
    }

    /// The project as it is on disk, after an earlier syncback gave IDs out.
    fn project() -> WeakDom {
        let mut old = WeakDom::new(InstanceBuilder::new("DataModel"));
        let storage = old.insert(
            old.root_ref(),
            InstanceBuilder::new("ReplicatedStorage").with_name("ReplicatedStorage"),
        );
        let rig = old.insert(
            storage,
            with_id(InstanceBuilder::new("Model").with_name("Rig"), "rig-id"),
        );
        old.insert(
            rig,
            with_id(InstanceBuilder::new("Part").with_name("Head"), "head-id"),
        );
        for suffix in ["a", "b"] {
            let twin = old.insert(
                storage,
                with_id(
                    InstanceBuilder::new("Part").with_name("Twin"),
                    &format!("twin-{suffix}"),
                ),
            );
            old.insert(
                twin,
                with_id(
                    InstanceBuilder::new("Attachment").with_name("Socket"),
                    &format!("socket-{suffix}"),
                ),
            );
            old.insert(
                storage,
                with_id(
                    InstanceBuilder::new("Part").with_name("Extra"),
                    &format!("extra-{suffix}"),
                ),
            );
        }
        old.insert(
            storage,
            with_id(
                InstanceBuilder::new("Part").with_name("Swapped"),
                "swapped-id",
            ),
        );
        old
    }

    /// The same place from Studio, where nothing carries a `Rojo_Id` yet.
    fn upload() -> WeakDom {
        let mut new = WeakDom::new(InstanceBuilder::new("DataModel"));
        let storage = new.insert(
            new.root_ref(),
            InstanceBuilder::new("ReplicatedStorage").with_name("ReplicatedStorage"),
        );
        let rig = new.insert(storage, InstanceBuilder::new("Model").with_name("Rig"));
        new.insert(rig, InstanceBuilder::new("Part").with_name("Head"));
        for _ in 0..2 {
            let twin = new.insert(storage, InstanceBuilder::new("Part").with_name("Twin"));
            new.insert(twin, InstanceBuilder::new("Attachment").with_name("Socket"));
        }
        // There were two on disk, so there's no telling which one is new.
        for _ in 0..3 {
            new.insert(storage, InstanceBuilder::new("Part").with_name("Extra"));
        }
        // Became a different class, so it's a different instance.
        new.insert(storage, InstanceBuilder::new("Folder").with_name("Swapped"));
        new
    }

    fn id_for<'a>(ids: &'a HashMap<Ref, String>, referent: &Ref) -> Option<&'a str> {
        ids.get(referent).map(String::as_str)
    }

    #[test]
    fn instances_keep_the_ids_they_have_on_disk() {
        let new = upload();
        let ids = ids_on_disk(&new, &project());

        assert_eq!(
            ids.get(&named(&new, "Rig")[0]).map(String::as_str),
            Some("rig-id")
        );
        assert_eq!(
            ids.get(&named(&new, "Head")[0]).map(String::as_str),
            Some("head-id")
        );
    }

    #[test]
    fn siblings_sharing_a_name_are_matched_in_order() {
        let new = upload();
        let ids = ids_on_disk(&new, &project());

        let twins = named(&new, "Twin");
        assert_eq!(id_for(&ids, &twins[0]), Some("twin-a"));
        assert_eq!(id_for(&ids, &twins[1]), Some("twin-b"));
        // Their children are matched too.
        let sockets = named(&new, "Socket");
        assert_eq!(id_for(&ids, &sockets[0]), Some("socket-a"));
        assert_eq!(id_for(&ids, &sockets[1]), Some("socket-b"));
    }

    #[test]
    fn ids_are_not_guessed_for_changed_counts_or_classes() {
        let new = upload();
        let ids = ids_on_disk(&new, &project());

        for referent in named(&new, "Extra")
            .into_iter()
            .chain(named(&new, "Swapped"))
        {
            assert_eq!(ids.get(&referent), None);
        }
        // Rig, Head, two Twins, and two Sockets.
        assert_eq!(ids.len(), 6);
    }

    #[test]
    fn ids_an_instance_already_has_are_kept_and_never_duplicated() {
        let mut new = upload();
        let rig = named(&new, "Rig")[0];
        let mut attributes = Attributes::new();
        attributes.insert(
            REF_ID_ATTRIBUTE_NAME.into(),
            Variant::String("own-id".into()),
        );
        new.get_by_ref_mut(rig)
            .unwrap()
            .properties
            .insert(ustr("Attributes"), attributes.into());
        // Another instance already holds the ID Head has on disk.
        let storage = named(&new, "ReplicatedStorage")[0];
        new.insert(
            storage,
            with_id(
                InstanceBuilder::new("Folder").with_name("Holder"),
                "head-id",
            ),
        );

        let ids = ids_on_disk(&new, &project());
        assert_eq!(id_for(&ids, &rig), None);
        assert_eq!(id_for(&ids, &named(&new, "Head")[0]), None);
        assert!(!ids.values().any(|id| id == "own-id" || id == "head-id"));
    }

    #[test]
    fn linking_uses_the_id_on_disk_for_a_target_without_one() {
        let mut new = upload();
        let rig = named(&new, "Rig")[0];
        let head = named(&new, "Head")[0];
        new.get_by_ref_mut(rig)
            .unwrap()
            .properties
            .insert(ustr("PrimaryPart"), Variant::Ref(head));
        // Extra is also referenced, but has no match on disk, so it still gets a
        // made-up ID.
        let extra = named(&new, "Extra")[0];
        new.get_by_ref_mut(rig)
            .unwrap()
            .properties
            .insert(ustr("Extra"), Variant::Ref(extra));

        let links = collect_referents(&new);
        let ids = ids_on_disk(&new, &project());
        link_referents(links, &mut new, &ids).unwrap();

        assert_eq!(
            get_existing_id(new.get_by_ref(head).unwrap()),
            Some("head-id")
        );
        let extra_id = get_existing_id(new.get_by_ref(extra).unwrap()).unwrap();
        assert!(!extra_id.starts_with("extra-"));
        let Some(Variant::Attributes(attributes)) = new
            .get_by_ref(rig)
            .unwrap()
            .properties
            .get(&ustr("Attributes"))
        else {
            panic!("Rig should have link attributes");
        };
        assert_eq!(
            attributes.get("Rojo_Target_PrimaryPart"),
            Some(&Variant::String("head-id".into()))
        );
    }

    #[test]
    fn finding_ids_on_disk_is_pure() {
        let new = upload();
        let old = project();
        let sorted = |ids: HashMap<Ref, String>| {
            let mut ids: Vec<_> = ids.into_values().collect();
            ids.sort();
            ids
        };
        assert_eq!(
            sorted(ids_on_disk(&new, &old)),
            sorted(ids_on_disk(&new, &old))
        );
    }
}
