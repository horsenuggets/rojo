//! Rebuilds a place from what the Studio plugin uploads for
//! `rojo syncback --from-studio`.
//!
//! Studio can't serialize services directly, so the plugin serializes the
//! children of every service the project maps in a single
//! `SerializationService:SerializeInstancesAsync` call. A single call keeps
//! references between services intact, which separate calls would turn into
//! `nil`. The buffer arrives in parts, and a manifest records which service each
//! top-level child belongs to.

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use rbx_dom_weak::{types::Variant, ustr, InstanceBuilder, WeakDom};
use serde::{Deserialize, Serialize};

/// A service the plugin should send the children of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioService {
    pub name: String,
    pub class_name: String,
}

/// One top-level child the plugin passed to `SerializeInstancesAsync`, in the
/// order it was passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub service: String,
    pub name: String,
    pub class_name: String,
}

/// The place rebuilt from an upload.
#[derive(Debug)]
pub struct AssembledPlace {
    pub dom: WeakDom,

    /// Manifest entries that Studio left out of the serialized buffer.
    pub dropped: Vec<ManifestEntry>,
}

/// Returns the services a project maps, which are the children of its
/// `DataModel` root.
pub fn studio_services(tree: &WeakDom) -> anyhow::Result<Vec<StudioService>> {
    let root = tree.root();
    if root.class != "DataModel" {
        bail!(
            "Syncback from Studio needs a project whose root is a DataModel, but this project's \
             root is a {}.",
            root.class
        );
    }

    Ok(root
        .children()
        .iter()
        .map(|child| {
            let child = tree.get_by_ref(*child).unwrap();
            StudioService {
                name: child.name.clone(),
                class_name: child.class.to_string(),
            }
        })
        .collect())
}

/// The most parts one upload can have. Parts are at most a few megabytes, so this
/// bounds how much memory an upload can make the server hold.
pub const MAX_PARTS: u32 = 256;

/// Collects the parts of an upload, which can arrive in any order.
#[derive(Debug, Default)]
pub struct UploadParts {
    parts: BTreeMap<u32, Vec<u8>>,
}

impl UploadParts {
    pub fn insert(&mut self, index: u32, data: Vec<u8>) -> anyhow::Result<()> {
        if index >= MAX_PARTS {
            bail!("Part {index} is past the limit of {MAX_PARTS} parts per upload.");
        }
        self.parts.insert(index, data);
        Ok(())
    }

    /// Joins the parts once all of `0..part_count` have arrived.
    pub fn join(&self, part_count: u32) -> anyhow::Result<Vec<u8>> {
        if part_count > MAX_PARTS {
            bail!("The upload declared {part_count} parts, but the limit is {MAX_PARTS}.");
        }
        if let Some(extra) = self.parts.keys().find(|index| **index >= part_count) {
            bail!("The upload has part {extra}, but it only declared {part_count} parts.");
        }
        // Every stored index is below `part_count` at this point, so any shortfall
        // in the count is a missing part.
        let missing = part_count as usize - self.parts.len();
        if missing > 0 {
            bail!(
                "The upload is missing {missing} of {part_count} {}.",
                if part_count == 1 { "part" } else { "parts" }
            );
        }

        Ok(self.parts.values().flatten().copied().collect())
    }
}

/// Decodes an uploaded buffer and puts each top-level child under its service.
///
/// Studio silently skips some instances when serializing (such as `Player`
/// objects), so the decoded children are matched against the manifest in order,
/// and entries with no matching child are reported as dropped.
///
/// `current_camera` locates `Workspace.CurrentCamera`. Its first element is a
/// manifest index, and the rest are child indices below that instance. Studio
/// can't serialize the Workspace service, so this is how syncback still knows
/// which camera to leave out.
///
/// An empty `bytes` is a place whose mapped services are all empty.
pub fn assemble_place(
    bytes: &[u8],
    services: &[StudioService],
    manifest: &[ManifestEntry],
    current_camera: Option<&[usize]>,
) -> anyhow::Result<AssembledPlace> {
    let mut dom = if bytes.is_empty() {
        WeakDom::new(InstanceBuilder::new("DataModel"))
    } else {
        rbx_binary::from_reader(bytes).context("Could not decode the place from Studio")?
    };
    let root_ref = dom.root_ref();
    dom.root_mut().class = ustr("DataModel");
    dom.root_mut().name = "DataModel".to_owned();

    let children = dom.root().children().to_vec();

    let mut service_refs = BTreeMap::new();
    for service in services {
        let referent = dom.insert(
            root_ref,
            InstanceBuilder::new(service.class_name.as_str()).with_name(service.name.as_str()),
        );
        service_refs.insert(service.name.as_str(), referent);
    }

    let (camera_entry, camera_path) = match current_camera {
        Some([first, rest @ ..]) => (Some(*first), rest),
        _ => (None, &[][..]),
    };

    let mut dropped = Vec::new();
    let mut camera = None;
    let mut entries = manifest.iter().enumerate();
    for child_ref in children {
        let child = dom.get_by_ref(child_ref).unwrap();
        let entry = loop {
            let Some((index, entry)) = entries.next() else {
                bail!(
                    "Studio sent an instance named \"{}\" ({}) that isn't in the upload manifest.",
                    child.name,
                    child.class
                );
            };
            if entry.name == child.name && entry.class_name == child.class.as_str() {
                if camera_entry == Some(index) {
                    camera = Some((entry.service.as_str(), child_ref));
                }
                break entry;
            }
            dropped.push(entry.clone());
        };

        let Some(service_ref) = service_refs.get(entry.service.as_str()) else {
            bail!(
                "The upload manifest puts \"{}\" in \"{}\", which the project doesn't map.",
                entry.name,
                entry.service
            );
        };
        dom.transfer_within(child_ref, *service_ref);
    }
    dropped.extend(entries.map(|(_, entry)| entry.clone()));

    if let Some((service, top_ref)) = camera {
        let camera_ref = camera_path.iter().try_fold(top_ref, |parent, index| {
            dom.get_by_ref(parent)?.children().get(*index).copied()
        });
        // Syncback deletes whatever `CurrentCamera` points at, so a path that
        // resolves to anything other than a camera is ignored.
        let camera_ref = camera_ref.filter(|r| {
            dom.get_by_ref(*r)
                .is_some_and(|inst| inst.class == "Camera")
        });
        let workspace = dom.get_by_ref_mut(service_refs[service]).unwrap();
        if let Some(camera_ref) = camera_ref.filter(|_| workspace.class == "Workspace") {
            workspace
                .properties
                .insert(ustr("CurrentCamera"), Variant::Ref(camera_ref));
        }
    }

    Ok(AssembledPlace { dom, dropped })
}

/// Copies each service's properties from the project's tree, keeping any the
/// upload already set (like `Workspace.CurrentCamera`).
///
/// Studio can't serialize services themselves, so the uploaded services have no
/// properties. Without this, syncback would treat every property a project sets
/// on a service (like `Lighting.Ambient`) as removed.
pub fn inherit_service_properties(new: &mut WeakDom, old: &WeakDom) {
    let old_services: BTreeMap<_, _> = old
        .root()
        .children()
        .iter()
        .map(|r| {
            let inst = old.get_by_ref(*r).unwrap();
            (inst.name.as_str(), inst)
        })
        .collect();

    for service_ref in new.root().children().to_vec() {
        let service = new.get_by_ref_mut(service_ref).unwrap();
        if let Some(old_service) = old_services.get(service.name.as_str()) {
            for (name, value) in &old_service.properties {
                service
                    .properties
                    .entry(*name)
                    .or_insert_with(|| value.clone());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::types::Ref;

    fn service(name: &str) -> StudioService {
        StudioService {
            name: name.to_owned(),
            class_name: name.to_owned(),
        }
    }

    fn entry(service: &str, name: &str, class_name: &str) -> ManifestEntry {
        ManifestEntry {
            service: service.to_owned(),
            name: name.to_owned(),
            class_name: class_name.to_owned(),
        }
    }

    /// Serializes `children` the way `SerializeInstancesAsync` would, as top-level
    /// instances of one model buffer.
    fn serialize(dom: &WeakDom, children: &[Ref]) -> Vec<u8> {
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, dom, children).unwrap();
        bytes
    }

    fn child_names(dom: &WeakDom, parent: Ref) -> Vec<String> {
        dom.get_by_ref(parent)
            .unwrap()
            .children()
            .iter()
            .map(|r| dom.get_by_ref(*r).unwrap().name.clone())
            .collect()
    }

    fn find_child(dom: &WeakDom, parent: Ref, name: &str) -> Ref {
        *dom.get_by_ref(parent)
            .unwrap()
            .children()
            .iter()
            .find(|r| dom.get_by_ref(**r).unwrap().name == name)
            .unwrap()
    }

    #[test]
    fn services_come_from_the_datamodel_root() {
        let mut tree = WeakDom::new(InstanceBuilder::new("DataModel"));
        tree.insert(
            tree.root_ref(),
            InstanceBuilder::new("ReplicatedStorage").with_name("ReplicatedStorage"),
        );
        tree.insert(
            tree.root_ref(),
            InstanceBuilder::new("Workspace").with_name("Workspace"),
        );

        assert_eq!(
            studio_services(&tree).unwrap(),
            vec![service("ReplicatedStorage"), service("Workspace")]
        );
    }

    #[test]
    fn model_projects_are_rejected() {
        let tree = WeakDom::new(InstanceBuilder::new("Folder"));
        assert!(studio_services(&tree).is_err());
    }

    #[test]
    fn parts_join_in_index_order_regardless_of_arrival() {
        let mut parts = UploadParts::default();
        parts.insert(2, vec![5, 6]).unwrap();
        parts.insert(0, vec![1, 2]).unwrap();
        parts.insert(1, vec![3, 4]).unwrap();

        assert_eq!(parts.join(3).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn missing_or_extra_parts_are_errors() {
        let mut parts = UploadParts::default();
        parts.insert(0, vec![1]).unwrap();
        parts.insert(2, vec![3]).unwrap();

        assert!(parts.join(3).is_err());
        assert!(parts.join(2).is_err());
        assert!(parts.join(1).is_err());
    }

    #[test]
    fn an_empty_upload_joins_to_nothing() {
        assert_eq!(UploadParts::default().join(0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn part_counts_and_indices_are_bounded() {
        let mut parts = UploadParts::default();
        assert!(parts.insert(MAX_PARTS, vec![1]).is_err());
        assert!(parts.insert(MAX_PARTS - 1, vec![1]).is_ok());

        // A huge declared count is rejected up front instead of being walked.
        assert!(UploadParts::default().join(u32::MAX).is_err());
        assert!(UploadParts::default().join(MAX_PARTS + 1).is_err());
    }

    #[test]
    fn an_empty_place_has_only_its_services() {
        let place = assemble_place(
            &[],
            &[service("ReplicatedStorage"), service("Workspace")],
            &[],
            None,
        )
        .unwrap();
        let dom = &place.dom;
        assert_eq!(
            child_names(dom, dom.root_ref()),
            vec!["ReplicatedStorage", "Workspace"]
        );
        for service in dom.root().children() {
            assert!(dom.get_by_ref(*service).unwrap().children().is_empty());
        }
    }

    #[test]
    fn children_are_placed_under_their_services() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let a = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("A"),
        );
        let b = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Part").with_name("B"),
        );
        let c = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("C"),
        );

        let place = assemble_place(
            &serialize(&source, &[a, b, c]),
            &[service("ReplicatedStorage"), service("Workspace")],
            &[
                entry("ReplicatedStorage", "A", "Folder"),
                entry("Workspace", "B", "Part"),
                entry("Workspace", "C", "Folder"),
            ],
            None,
        )
        .unwrap();

        let dom = &place.dom;
        assert_eq!(dom.root().class, "DataModel");
        assert_eq!(
            child_names(dom, dom.root_ref()),
            vec!["ReplicatedStorage", "Workspace"]
        );
        let storage = find_child(dom, dom.root_ref(), "ReplicatedStorage");
        let workspace = find_child(dom, dom.root_ref(), "Workspace");
        assert_eq!(child_names(dom, storage), vec!["A"]);
        assert_eq!(child_names(dom, workspace), vec!["B", "C"]);
        assert!(place.dropped.is_empty());
    }

    #[test]
    fn references_between_services_survive() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let target = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Part").with_name("Target"),
        );
        let pointer = source.insert(
            source.root_ref(),
            InstanceBuilder::new("ObjectValue")
                .with_name("Pointer")
                .with_property("Value", Variant::Ref(target)),
        );

        let place = assemble_place(
            &serialize(&source, &[target, pointer]),
            &[service("ReplicatedStorage"), service("Workspace")],
            &[
                entry("Workspace", "Target", "Part"),
                entry("ReplicatedStorage", "Pointer", "ObjectValue"),
            ],
            None,
        )
        .unwrap();

        let dom = &place.dom;
        let storage = find_child(dom, dom.root_ref(), "ReplicatedStorage");
        let pointer = dom.get_by_ref(find_child(dom, storage, "Pointer")).unwrap();
        let Some(Variant::Ref(value)) = pointer.properties.get(&ustr("Value")) else {
            panic!("Pointer.Value should be a Ref");
        };
        let target = dom
            .get_by_ref(*value)
            .expect("Pointer.Value should resolve");
        assert_eq!(target.name, "Target");
        assert_eq!(dom.get_by_ref(target.parent()).unwrap().name, "Workspace");
    }

    #[test]
    fn instances_studio_skipped_are_reported_as_dropped() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let kept = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("Kept"),
        );

        let place = assemble_place(
            &serialize(&source, &[kept]),
            &[service("Players"), service("Workspace")],
            &[
                entry("Players", "Player1", "Player"),
                entry("Workspace", "Kept", "Folder"),
                entry("Workspace", "Trailing", "Folder"),
            ],
            None,
        )
        .unwrap();

        let dom = &place.dom;
        let workspace = find_child(dom, dom.root_ref(), "Workspace");
        assert_eq!(child_names(dom, workspace), vec!["Kept"]);
        assert_eq!(
            place.dropped,
            vec![
                entry("Players", "Player1", "Player"),
                entry("Workspace", "Trailing", "Folder"),
            ]
        );
    }

    #[test]
    fn instances_missing_from_the_manifest_are_errors() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let a = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("A"),
        );

        let result = assemble_place(
            &serialize(&source, &[a]),
            &[service("Workspace")],
            &[entry("Workspace", "B", "Folder")],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn manifest_services_must_be_mapped() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let a = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("A"),
        );

        let result = assemble_place(
            &serialize(&source, &[a]),
            &[service("Workspace")],
            &[entry("Lighting", "A", "Folder")],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn service_properties_come_from_the_project() {
        let mut old = WeakDom::new(InstanceBuilder::new("DataModel"));
        old.insert(
            old.root_ref(),
            InstanceBuilder::new("Lighting")
                .with_name("Lighting")
                .with_property("Brightness", Variant::Float32(3.0)),
        );
        let mut new = WeakDom::new(InstanceBuilder::new("DataModel"));
        let lighting = new.insert(
            new.root_ref(),
            InstanceBuilder::new("Lighting").with_name("Lighting"),
        );
        let workspace = new.insert(
            new.root_ref(),
            InstanceBuilder::new("Workspace").with_name("Workspace"),
        );

        inherit_service_properties(&mut new, &old);

        assert_eq!(
            new.get_by_ref(lighting)
                .unwrap()
                .properties
                .get(&ustr("Brightness")),
            Some(&Variant::Float32(3.0))
        );
        assert!(new.get_by_ref(workspace).unwrap().properties.is_empty());
    }

    #[test]
    fn assembly_is_deterministic() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let a = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("A"),
        );
        let b = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("B"),
        );
        let bytes = serialize(&source, &[a, b]);
        let services = [service("ReplicatedStorage"), service("Workspace")];
        let manifest = [
            entry("Workspace", "A", "Folder"),
            entry("ReplicatedStorage", "B", "Folder"),
        ];

        let layout = |place: AssembledPlace| {
            let dom = place.dom;
            child_names(&dom, dom.root_ref())
                .iter()
                .map(|s| {
                    (
                        s.clone(),
                        child_names(&dom, find_child(&dom, dom.root_ref(), s)),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            layout(assemble_place(&bytes, &services, &manifest, None).unwrap()),
            layout(assemble_place(&bytes, &services, &manifest, None).unwrap())
        );
    }

    fn camera_upload() -> (Vec<u8>, Vec<ManifestEntry>) {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let part = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Part").with_name("Part"),
        );
        let camera = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Camera").with_name("Camera"),
        );
        (
            serialize(&source, &[part, camera]),
            vec![
                entry("Workspace", "Part", "Part"),
                entry("Workspace", "Camera", "Camera"),
            ],
        )
    }

    fn current_camera(dom: &WeakDom) -> Option<String> {
        let workspace = find_child(dom, dom.root_ref(), "Workspace");
        match dom
            .get_by_ref(workspace)
            .unwrap()
            .properties
            .get(&ustr("CurrentCamera"))
        {
            Some(Variant::Ref(camera)) => Some(dom.get_by_ref(*camera).unwrap().name.clone()),
            _ => None,
        }
    }

    #[test]
    fn current_camera_points_at_the_uploaded_camera() {
        let (bytes, manifest) = camera_upload();
        let place = assemble_place(&bytes, &[service("Workspace")], &manifest, Some(&[1])).unwrap();
        assert_eq!(current_camera(&place.dom).as_deref(), Some("Camera"));

        let place = assemble_place(&bytes, &[service("Workspace")], &manifest, None).unwrap();
        assert_eq!(current_camera(&place.dom), None);
    }

    #[test]
    fn nested_current_cameras_are_found_by_path() {
        let mut source = WeakDom::new(InstanceBuilder::new("DataModel"));
        let folder = source.insert(
            source.root_ref(),
            InstanceBuilder::new("Folder").with_name("Cameras"),
        );
        source.insert(folder, InstanceBuilder::new("Part").with_name("Marker"));
        source.insert(folder, InstanceBuilder::new("Camera").with_name("Nested"));
        let bytes = serialize(&source, &[folder]);
        let manifest = [entry("Workspace", "Cameras", "Folder")];

        let place =
            assemble_place(&bytes, &[service("Workspace")], &manifest, Some(&[0, 1])).unwrap();
        assert_eq!(current_camera(&place.dom).as_deref(), Some("Nested"));
    }

    #[test]
    fn current_camera_paths_that_miss_a_camera_are_ignored() {
        let (bytes, mut manifest) = camera_upload();
        manifest.insert(1, entry("Workspace", "Dropped", "Camera"));
        // Each path either names an entry Studio dropped, runs past the manifest or
        // a child list, or lands on something that isn't a camera.
        let paths: [&[usize]; 5] = [&[1], &[9], &[2, 0], &[0], &[]];
        for path in paths {
            let place =
                assemble_place(&bytes, &[service("Workspace")], &manifest, Some(path)).unwrap();
            assert_eq!(current_camera(&place.dom), None, "path {path:?}");
        }
    }

    #[test]
    fn inherited_properties_never_replace_uploaded_ones() {
        let mut old = WeakDom::new(InstanceBuilder::new("DataModel"));
        old.insert(
            old.root_ref(),
            InstanceBuilder::new("Workspace")
                .with_name("Workspace")
                .with_property("Gravity", Variant::Float32(50.0))
                .with_property("CurrentCamera", Variant::Ref(Ref::none())),
        );
        let (bytes, manifest) = camera_upload();
        let mut place =
            assemble_place(&bytes, &[service("Workspace")], &manifest, Some(&[1])).unwrap();

        inherit_service_properties(&mut place.dom, &old);

        let workspace = find_child(&place.dom, place.dom.root_ref(), "Workspace");
        assert_eq!(
            place
                .dom
                .get_by_ref(workspace)
                .unwrap()
                .properties
                .get(&ustr("Gravity")),
            Some(&Variant::Float32(50.0))
        );
        assert_eq!(current_camera(&place.dom).as_deref(), Some("Camera"));
    }
}
