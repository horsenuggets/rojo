use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Instance, InstanceBuilder, Ustr, UstrMap, WeakDom,
};

use crate::{multimap::MultiMap, RojoRef, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX};

/// Reads an attribute that holds an ID, which files store as a string.
fn attribute_string(value: &Variant) -> Option<String> {
    match value {
        Variant::String(value) => Some(value.clone()),
        Variant::BinaryString(value) => std::str::from_utf8(value.as_ref()).ok().map(str::to_owned),
        _ => None,
    }
}

use super::{InstanceMetadata, InstanceSnapshot};

/// The referents to give a snapshot's instances, shaped like the snapshot. See
/// [`RojoTree::insert_instance_with_referents`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotReferents {
    pub id: Option<Ref>,
    pub children: Vec<SnapshotReferents>,
}

/// An expanded variant of rbx_dom_weak's `WeakDom` that tracks additional
/// metadata per instance that's Rojo-specific.
///
/// This tree is also optimized for doing fast incremental updates and patches.
#[derive(Debug)]
pub struct RojoTree {
    /// Contains the instances without their Rojo-specific metadata.
    inner: WeakDom,

    /// Metadata associated with each instance that is kept up-to-date with the
    /// set of actual instances.
    metadata_map: HashMap<Ref, InstanceMetadata>,

    /// A multimap from source paths to all of the root instances that were
    /// constructed from that path.
    ///
    /// Descendants of those instances should not be contained in the set, the
    /// value portion of the map is also a set in order to support the same path
    /// appearing multiple times in the same Rojo project. This is sometimes
    /// called "path aliasing" in various Rojo documentation.
    path_to_ids: MultiMap<PathBuf, Ref>,

    /// A map of specified RojoRefs to underlying Refs they represent.
    /// This field is a MultiMap to allow for the possibility of the user specifying
    /// the same RojoRef for multiple different instances. An entry containing
    /// multiple elements is an error condition that should be raised to the user.
    specified_id_to_refs: MultiMap<RojoRef, Ref>,
}

impl RojoTree {
    pub fn new(snapshot: InstanceSnapshot) -> RojoTree {
        let root_builder = InstanceBuilder::new(snapshot.class_name)
            .with_name(snapshot.name)
            .with_properties(snapshot.properties);

        let mut tree = RojoTree {
            inner: WeakDom::new(root_builder),
            metadata_map: HashMap::new(),
            path_to_ids: MultiMap::new(),
            specified_id_to_refs: MultiMap::new(),
        };

        let root_ref = tree.inner.root_ref();

        tree.insert_metadata(root_ref, snapshot.metadata);

        for child in snapshot.children {
            tree.insert_instance(root_ref, child);
        }

        tree
    }

    /// Copies the serialization baseline without sharing mutable state or changing IDs.
    /// Syncback filters this baseline in place, so it must not be the live tree.
    pub(crate) fn clone_for_serialization(&self) -> Self {
        let builder = |id| {
            let instance = self.inner.get_by_ref(id).unwrap();
            InstanceBuilder::new(instance.class)
                .with_name(&instance.name)
                .with_properties(instance.properties.clone())
                .with_referent(id)
        };
        let root = self.get_root_id();
        let mut inner = WeakDom::new(builder(root));
        let mut pending = vec![root];
        while let Some(parent) = pending.pop() {
            for &child in self.inner.get_by_ref(parent).unwrap().children() {
                inner.insert(parent, builder(child));
                pending.push(child);
            }
        }
        Self {
            inner,
            metadata_map: self.metadata_map.clone(),
            path_to_ids: self.path_to_ids.clone(),
            specified_id_to_refs: self.specified_id_to_refs.clone(),
        }
    }

    pub fn inner(&self) -> &WeakDom {
        &self.inner
    }

    pub fn get_root_id(&self) -> Ref {
        self.inner.root_ref()
    }

    /// Returns the root Instance of this tree.
    #[inline]
    pub fn root(&self) -> InstanceWithMeta<'_> {
        self.get_instance(self.get_root_id())
            .expect("RojoTrees should have a root")
    }

    pub fn get_instance(&self, id: Ref) -> Option<InstanceWithMeta<'_>> {
        if let Some(instance) = self.inner.get_by_ref(id) {
            let metadata = self.metadata_map.get(&id).unwrap();

            Some(InstanceWithMeta { instance, metadata })
        } else {
            None
        }
    }

    pub fn get_instance_mut(&mut self, id: Ref) -> Option<InstanceWithMetaMut<'_>> {
        if let Some(instance) = self.inner.get_by_ref_mut(id) {
            let metadata = self.metadata_map.get_mut(&id).unwrap();

            Some(InstanceWithMetaMut { instance, metadata })
        } else {
            None
        }
    }

    pub fn insert_instance(&mut self, parent_ref: Ref, snapshot: InstanceSnapshot) -> Ref {
        self.insert_instance_inner(parent_ref, snapshot, None, &mut Vec::new())
    }

    /// Inserts `snapshot` like [`Self::insert_instance`], but gives its instances
    /// the referents in `referents`, which is shaped like the snapshot. An
    /// instance without one, or beyond the shape, gets a new referent. The
    /// referents must not be in the tree already.
    ///
    /// Each inserted instance is recorded in `inserted` as its snapshot's
    /// `snapshot_id` and the referent it got. Pass the list to
    /// [`Self::resolve_snapshot_refs`] once everything is inserted.
    pub fn insert_instance_with_referents(
        &mut self,
        parent_ref: Ref,
        snapshot: InstanceSnapshot,
        referents: &SnapshotReferents,
        inserted: &mut Vec<(Ref, Ref)>,
    ) -> Ref {
        self.insert_instance_inner(parent_ref, snapshot, Some(referents), inserted)
    }

    /// Resolves the references of the instances in `inserted`, a list of
    /// snapshot ids and referents, the way applying a patch does. A file's
    /// references hold snapshot ids, which now point at the instances they
    /// became, or are `Rojo_Target_*` attributes naming a `Rojo_Id`.
    pub fn resolve_snapshot_refs(&mut self, inserted: &[(Ref, Ref)]) {
        let resolved: HashMap<Ref, Ref> = inserted
            .iter()
            .copied()
            .filter(|(snapshot_id, _)| snapshot_id.is_some())
            .collect();
        for &(_, referent) in inserted {
            let Some(instance) = self.inner.get_by_ref_mut(referent) else {
                continue;
            };
            for value in instance.properties.values_mut() {
                if let Variant::Ref(target) = value {
                    if let Some(&referent) = resolved.get(target) {
                        *target = referent;
                    }
                }
            }
        }

        // Every ID is registered before any target is looked up, since a
        // target can come later in the list than what points at it.
        let mut targets = Vec::new();
        for &(_, referent) in inserted {
            let Some(Variant::Attributes(attributes)) = self
                .inner
                .get_by_ref(referent)
                .and_then(|instance| instance.properties.get(&ustr("Attributes")))
            else {
                continue;
            };
            let specified = attributes
                .get(REF_ID_ATTRIBUTE_NAME)
                .and_then(attribute_string);
            for (name, value) in attributes.iter() {
                if let (Some(property), Some(target)) = (
                    name.strip_prefix(REF_POINTER_ATTRIBUTE_PREFIX),
                    attribute_string(value),
                ) {
                    targets.push((referent, ustr(property), RojoRef::new(target)));
                }
            }
            if let Some(specified) = specified {
                self.set_specified_id(referent, RojoRef::new(specified));
            }
        }
        for (referent, property, target) in targets {
            if let Some(target) = self.get_specified_id(&target) {
                if let Some(instance) = self.inner.get_by_ref_mut(referent) {
                    instance.properties.insert(property, Variant::Ref(target));
                }
            }
        }
    }

    fn insert_instance_inner(
        &mut self,
        parent_ref: Ref,
        snapshot: InstanceSnapshot,
        referents: Option<&SnapshotReferents>,
        inserted: &mut Vec<(Ref, Ref)>,
    ) -> Ref {
        let snapshot_id = snapshot.snapshot_id;
        // !!!!!!!!!! UGLY HACK !!!!!!!!!!
        // ! If you are going to change this, go change it in patch_compute/compute_property_patches too
        //
        // This is a set of special cases working around a more general problem upstream
        // in rbx-dom that causes pivots to not build to file correctly, described in
        // github.com/rojo-rbx/rojo/issues/628 and github.com/rojo-rbx/rbx-dom/issues/385
        //
        // We need to insert the NeedsPivotMigration property with a value of false on
        // every instance that inherits from Model for pivots to build correctly.
        let hack_needs_pivot_migration = match snapshot.class_name.as_ref() {
            // This is not a future proof way to do this but the last time a
            // descendant of Model was added was in 2020 so it's probably fine.
            "Model" | "Actor" | "Tool" | "HopperBin" | "Flag" | "WorldModel" | "Workspace"
            | "Status"
                if !snapshot
                    .properties
                    .contains_key(&ustr("NeedsPivotMigration")) =>
            {
                vec![("NeedsPivotMigration", Variant::Bool(false))]
            }
            _ => Vec::new(),
        };

        let mut builder = InstanceBuilder::empty()
            .with_class(snapshot.class_name)
            .with_name(snapshot.name.into_owned())
            .with_properties(hack_needs_pivot_migration)
            .with_properties(snapshot.properties);
        if let Some(chosen) = referents.and_then(|referents| referents.id) {
            builder = builder.with_referent(chosen);
        }

        let referent = self.inner.insert(parent_ref, builder);
        self.insert_metadata(referent, snapshot.metadata);
        inserted.push((snapshot_id, referent));

        for (index, child) in snapshot.children.into_iter().enumerate() {
            let child_referents = referents.and_then(|referents| referents.children.get(index));
            self.insert_instance_inner(referent, child, child_referents, inserted);
        }

        referent
    }

    /// Reparents an existing subtree without changing its referents or metadata indexes.
    /// Unlike `WeakDom::transfer_within`, this rejects detached nodes and cycles.
    pub fn reparent(&mut self, id: Ref, parent: Ref) -> anyhow::Result<()> {
        anyhow::ensure!(id != self.get_root_id(), "Cannot reparent the tree root");
        let instance = self
            .get_instance(id)
            .ok_or_else(|| anyhow::anyhow!("Reparented instance does not exist"))?;
        anyhow::ensure!(self.get_instance(parent).is_some(), "Parent does not exist");

        let mut ancestor = parent;
        while ancestor.is_some() {
            anyhow::ensure!(ancestor != id, "Reparenting would create a cycle");
            ancestor = self.get_instance(ancestor).unwrap().parent();
        }

        if instance.parent() != parent {
            self.inner.transfer_within(id, parent);
        }
        Ok(())
    }

    pub fn remove(&mut self, id: Ref) {
        let mut to_move = VecDeque::new();
        to_move.push_back(id);

        while let Some(id) = to_move.pop_front() {
            self.remove_metadata(id);

            if let Some(instance) = self.inner.get_by_ref(id) {
                to_move.extend(instance.children().iter().copied());
            }
        }

        self.inner.destroy(id);
    }

    /// Replaces the metadata associated with the given instance ID.
    pub fn update_metadata(&mut self, id: Ref, metadata: InstanceMetadata) {
        use std::collections::hash_map::Entry;

        match self.metadata_map.entry(id) {
            Entry::Occupied(mut entry) => {
                let existing_metadata = entry.get();

                // If this instance's source path changed, we need to update our
                // path associations so that file changes will trigger updates
                // to this instance correctly.
                if existing_metadata.relevant_paths != metadata.relevant_paths {
                    for existing_path in &existing_metadata.relevant_paths {
                        self.path_to_ids.remove(existing_path, id);
                    }

                    for new_path in &metadata.relevant_paths {
                        self.path_to_ids.insert(new_path.clone(), id);
                    }
                }
                if existing_metadata.specified_id != metadata.specified_id {
                    // We need to uphold the invariant that each ID can only map
                    // to one referent.
                    if let Some(new) = &metadata.specified_id {
                        if !self.specified_id_to_refs.get(new).is_empty() {
                            log::error!("Duplicate user-specified referent '{new}'");
                        }

                        self.specified_id_to_refs.insert(new.clone(), id);
                    }
                    if let Some(old) = &existing_metadata.specified_id {
                        self.specified_id_to_refs.remove(old, id);
                    }
                }

                entry.insert(metadata);
            }
            Entry::Vacant(entry) => {
                entry.insert(metadata);
            }
        }
    }

    pub fn descendants(&self, id: Ref) -> RojoDescendants<'_> {
        let mut queue = VecDeque::new();
        queue.push_back(id);

        RojoDescendants { queue, tree: self }
    }

    pub fn get_ids_at_path(&self, path: &Path) -> &[Ref] {
        self.path_to_ids.get(path)
    }

    pub fn get_metadata(&self, id: Ref) -> Option<&InstanceMetadata> {
        self.metadata_map.get(&id)
    }

    /// Returns every instance that holds the given RojoRef.
    pub fn specified_id_holders(&self, specified: &RojoRef) -> &[Ref] {
        self.specified_id_to_refs.get(specified)
    }

    /// Get the backing Ref of the given RojoRef. If the RojoRef maps to exactly
    /// one Ref, this method returns Some. Otherwise, it returns None.
    pub fn get_specified_id(&self, specified: &RojoRef) -> Option<Ref> {
        match self.specified_id_to_refs.get(specified)[..] {
            [referent] => Some(referent),
            _ => None,
        }
    }

    pub fn set_specified_id(&mut self, id: Ref, specified: RojoRef) {
        if let Some(metadata) = self.metadata_map.get_mut(&id) {
            if let Some(old) = metadata.specified_id.replace(specified.clone()) {
                self.specified_id_to_refs.remove(&old, id);
            }
        }
        self.specified_id_to_refs.insert(specified, id);
    }

    fn insert_metadata(&mut self, id: Ref, metadata: InstanceMetadata) {
        for path in &metadata.relevant_paths {
            self.path_to_ids.insert(path.clone(), id);
        }

        if let Some(specified_id) = &metadata.specified_id {
            if !self.specified_id_to_refs.get(specified_id).is_empty() {
                log::error!("Duplicate user-specified referent '{specified_id}'");
            }

            self.set_specified_id(id, specified_id.clone());
        }

        self.metadata_map.insert(id, metadata);
    }

    /// Moves the Rojo metadata from the instance with the given ID from this
    /// tree into some loose maps.
    fn remove_metadata(&mut self, id: Ref) {
        let metadata = self.metadata_map.remove(&id).unwrap();

        if let Some(specified) = metadata.specified_id {
            self.specified_id_to_refs.remove(&specified, id);
        }

        for path in &metadata.relevant_paths {
            self.path_to_ids.remove(path, id);
        }
    }
}

pub struct RojoDescendants<'a> {
    queue: VecDeque<Ref>,
    tree: &'a RojoTree,
}

impl<'a> Iterator for RojoDescendants<'a> {
    type Item = InstanceWithMeta<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.queue.pop_front()?;

        let instance = self
            .tree
            .inner
            .get_by_ref(id)
            .expect("Instance did not exist");

        let metadata = self
            .tree
            .get_metadata(instance.referent())
            .expect("Metadata did not exist for instance");

        self.queue.extend(instance.children().iter().copied());

        Some(InstanceWithMeta { instance, metadata })
    }
}

/// RojoTree's equivalent of `&'a Instance`.
///
/// This has to be a value type for RojoTree because the instance and metadata
/// are stored in different places. The mutable equivalent is
/// `InstanceWithMetaMut`.
#[derive(Debug, Clone, Copy)]
pub struct InstanceWithMeta<'a> {
    instance: &'a Instance,
    metadata: &'a InstanceMetadata,
}

impl<'a> InstanceWithMeta<'a> {
    pub fn id(&self) -> Ref {
        self.instance.referent()
    }

    pub fn parent(&self) -> Ref {
        self.instance.parent()
    }

    pub fn name(&self) -> &'a str {
        &self.instance.name
    }

    pub fn class_name(&self) -> Ustr {
        self.instance.class
    }

    pub fn properties(&self) -> &'a UstrMap<Variant> {
        &self.instance.properties
    }

    pub fn children(&self) -> &'a [Ref] {
        self.instance.children()
    }

    pub fn metadata(&self) -> &'a InstanceMetadata {
        self.metadata
    }

    pub fn inner(&self) -> &Instance {
        self.instance
    }
}

/// RojoTree's equivalent of `&'a mut Instance`.
///
/// This has to be a value type for RojoTree because the instance and metadata
/// are stored in different places. The immutable equivalent is
/// `InstanceWithMeta`.
#[derive(Debug)]
pub struct InstanceWithMetaMut<'a> {
    instance: &'a mut Instance,
    metadata: &'a mut InstanceMetadata,
}

impl InstanceWithMetaMut<'_> {
    pub fn id(&self) -> Ref {
        self.instance.referent()
    }

    pub fn name(&self) -> &str {
        &self.instance.name
    }

    pub fn name_mut(&mut self) -> &mut String {
        &mut self.instance.name
    }

    pub fn class_name(&self) -> &str {
        &self.instance.class
    }

    pub fn set_class_name<'a, S: Into<&'a str>>(&mut self, new_class: S) {
        self.instance.class = ustr(new_class.into());
    }

    pub fn properties(&self) -> &UstrMap<Variant> {
        &self.instance.properties
    }

    pub fn properties_mut(&mut self) -> &mut UstrMap<Variant> {
        &mut self.instance.properties
    }

    pub fn children(&self) -> &[Ref] {
        self.instance.children()
    }

    pub fn metadata(&self) -> &InstanceMetadata {
        self.metadata
    }

    pub fn inner(&self) -> &Instance {
        self.instance
    }

    pub fn inner_mut(&mut self) -> &mut Instance {
        self.instance
    }
}

#[cfg(test)]
mod test {
    use crate::{
        snapshot::{InstanceMetadata, InstanceSnapshot},
        RojoRef,
    };
    use rbx_dom_weak::{
        types::{Attributes, Ref, Variant},
        ustr, UstrMap,
    };

    use super::{RojoTree, SnapshotReferents};

    #[test]
    fn serialization_clone_preserves_referents_metadata_and_indexes() {
        let path = std::path::PathBuf::from("shared.luau");
        let specified = RojoRef::new("shared".into());
        let mut metadata = InstanceMetadata::new().specified_id(Some(specified.clone()));
        metadata.relevant_paths.push(path.clone());
        let mut tree = RojoTree::new(InstanceSnapshot::new().metadata(metadata.clone()));
        let root = tree.get_root_id();
        let child = tree.insert_instance(
            root,
            InstanceSnapshot::new().name("Child").metadata(metadata),
        );
        tree.get_instance_mut(root)
            .unwrap()
            .properties_mut()
            .insert(ustr("Target"), Variant::Ref(child));
        let mut detached = tree.clone_for_serialization();
        assert_eq!(detached.get_root_id(), root);
        for instance in tree.descendants(root) {
            let copy = detached.get_instance(instance.id()).unwrap();
            assert_eq!(copy.parent(), instance.parent());
            assert_eq!(copy.children(), instance.children());
            assert_eq!(copy.name(), instance.name());
            assert_eq!(copy.class_name(), instance.class_name());
            assert_eq!(copy.properties(), instance.properties());
            assert_eq!(copy.metadata(), instance.metadata());
        }
        assert_eq!(detached.get_ids_at_path(&path), [root, child]);
        assert_eq!(detached.specified_id_holders(&specified), [root, child]);
        assert_eq!(detached.get_specified_id(&specified), None);
        detached.remove(child);
        detached
            .get_instance_mut(root)
            .unwrap()
            .properties_mut()
            .clear();
        assert_eq!(detached.get_specified_id(&specified), Some(root));
        assert_eq!(tree.get_ids_at_path(&path), [root, child]);
        assert_eq!(tree.specified_id_holders(&specified), [root, child]);
        assert_eq!(tree.root().children(), [child]);
        assert_eq!(
            tree.root().properties()[&ustr("Target")],
            Variant::Ref(child)
        );
        assert!(tree.get_metadata(child).is_some());
    }

    #[test]
    fn inserts_with_chosen_referents_and_fills_the_rest() {
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let snapshot = InstanceSnapshot::new().name("Parent").children(vec![
            InstanceSnapshot::new().name("Chosen"),
            InstanceSnapshot::new().name("Unchosen"),
            InstanceSnapshot::new().name("Beyond"),
        ]);
        let parent = Ref::new();
        let chosen = Ref::new();
        let referents = SnapshotReferents {
            id: Some(parent),
            children: vec![
                SnapshotReferents {
                    id: Some(chosen),
                    children: Vec::new(),
                },
                SnapshotReferents::default(),
            ],
        };

        assert_eq!(
            tree.insert_instance_with_referents(root, snapshot, &referents, &mut Vec::new()),
            parent
        );
        let children = tree.get_instance(parent).unwrap().children().to_vec();
        assert_eq!(children.len(), 3);
        assert_eq!(children[0], chosen);
        assert_eq!(tree.get_instance(chosen).unwrap().name(), "Chosen");
        for child in &children[1..] {
            assert!(child.is_some() && *child != parent && *child != chosen);
        }
        assert_eq!(tree.get_instance(children[2]).unwrap().name(), "Beyond");
    }

    /// A file that can't hold references links them through attributes, and a
    /// target can be inserted after what points at it.
    #[test]
    fn resolves_attribute_references_between_inserted_snapshots() {
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let attributes = |entries: &[(&str, &str)]| {
            let mut attributes = Attributes::new();
            for (name, value) in entries {
                attributes.insert(name.to_string(), Variant::String(value.to_string()));
            }
            UstrMap::from_iter([(ustr("Attributes"), Variant::Attributes(attributes))])
        };
        let snapshot = InstanceSnapshot::new().name("Model").children(vec![
            InstanceSnapshot::new()
                .name("Weld")
                .properties(attributes(&[("Rojo_Target_Part0", "part")])),
            InstanceSnapshot::new()
                .name("Part")
                .properties(attributes(&[("Rojo_Id", "part")])),
        ]);

        let mut inserted = Vec::new();
        let model = tree.insert_instance_with_referents(
            root,
            snapshot,
            &SnapshotReferents::default(),
            &mut inserted,
        );
        tree.resolve_snapshot_refs(&inserted);

        let children = tree.get_instance(model).unwrap().children().to_vec();
        let part = children[1];
        assert_eq!(
            tree.get_instance(children[0])
                .unwrap()
                .properties()
                .get(&ustr("Part0")),
            Some(&Variant::Ref(part))
        );
        assert_eq!(
            tree.get_specified_id(&RojoRef::new("part".into())),
            Some(part)
        );
        assert_eq!(
            tree.get_metadata(part).unwrap().specified_id,
            Some(RojoRef::new("part".into()))
        );
    }

    #[test]
    fn resolves_references_between_inserted_snapshots() {
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let outside = tree.insert_instance(root, InstanceSnapshot::new().name("Outside"));
        let target_snapshot = Ref::new();
        let weld = |name: &str| {
            InstanceSnapshot::new()
                .name(name)
                .properties(UstrMap::from_iter([
                    (ustr("Part0"), Variant::Ref(target_snapshot)),
                    (ustr("Part1"), Variant::Ref(outside)),
                ]))
        };
        let snapshot = InstanceSnapshot::new().name("Model").children(vec![
            weld("Before"),
            InstanceSnapshot::new()
                .name("Target")
                .snapshot_id(target_snapshot),
            weld("After"),
        ]);
        let target = Ref::new();
        let referents = SnapshotReferents {
            id: None,
            children: vec![
                SnapshotReferents::default(),
                SnapshotReferents {
                    id: Some(target),
                    children: Vec::new(),
                },
            ],
        };

        let mut inserted = Vec::new();
        let model = tree.insert_instance_with_referents(root, snapshot, &referents, &mut inserted);
        tree.resolve_snapshot_refs(&inserted);

        for &child in tree.get_instance(model).unwrap().children() {
            let properties = tree.get_instance(child).unwrap().properties();
            if let Some(part0) = properties.get(&ustr("Part0")) {
                assert_eq!(part0, &Variant::Ref(target));
                assert_eq!(properties.get(&ustr("Part1")), Some(&Variant::Ref(outside)));
            }
        }
    }

    #[test]
    fn reports_which_instances_hold_a_specified_id() {
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let id = RojoRef::new("taken".into());
        assert!(tree.specified_id_holders(&id).is_empty());
        let mut metadata = InstanceMetadata::new();
        metadata.specified_id = Some(id.clone());
        let holder = tree.insert_instance(root, InstanceSnapshot::new().metadata(metadata));
        assert_eq!(tree.specified_id_holders(&id), [holder]);
        assert!(tree
            .specified_id_holders(&RojoRef::new("free".into()))
            .is_empty());
    }

    #[test]
    fn checked_reparent_rejects_invalid_moves_and_keeps_same_parent_order() {
        use rbx_dom_weak::types::Ref;
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let a = tree.insert_instance(root, InstanceSnapshot::new());
        let b = tree.insert_instance(a, InstanceSnapshot::new());
        let c = tree.insert_instance(root, InstanceSnapshot::new());
        for (id, parent) in [
            (root, a),
            (a, a),
            (a, b),
            (a, Ref::none()),
            (Ref::new(), root),
            (a, Ref::new()),
        ] {
            assert!(tree.reparent(id, parent).is_err());
            assert_eq!(tree.root().children(), &[a, c]);
            assert_eq!(tree.get_instance(a).unwrap().children(), &[b]);
        }
        tree.reparent(a, root).unwrap();
        assert_eq!(tree.root().children(), &[a, c]);
        tree.reparent(a, c).unwrap();
        assert_eq!(tree.get_instance(c).unwrap().children(), &[a]);
        assert_eq!(tree.get_instance(a).unwrap().children(), &[b]);
    }

    #[test]
    fn swap_duped_specified_ids() {
        let custom_ref = RojoRef::new("MyCoolRef".into());
        let snapshot = InstanceSnapshot::new()
            .metadata(InstanceMetadata::new().specified_id(Some(custom_ref.clone())));
        let mut tree = RojoTree::new(InstanceSnapshot::new());

        let original = tree.insert_instance(tree.get_root_id(), snapshot.clone());
        assert_eq!(tree.get_specified_id(&custom_ref.clone()), Some(original));

        let duped = tree.insert_instance(tree.get_root_id(), snapshot.clone());
        assert_eq!(tree.get_specified_id(&custom_ref.clone()), None);

        tree.remove(original);
        assert_eq!(tree.get_specified_id(&custom_ref.clone()), Some(duped));
    }
}
