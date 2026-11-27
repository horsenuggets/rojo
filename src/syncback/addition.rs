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
    snapshot::{InstanceWithMeta, RojoTree},
    Project,
};

use super::{
    file_names::{directory_middleware_for_instance, free_child_stem, sibling_number},
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
/// `directory`, which the new files must not take. `replacement` identifies the
/// old root, whose ordinal is kept only within the same parent and name group.
///
/// The output is the same as a full syncback would write for the subtree, so
/// the project's naming, layout, and ignore rules all apply. Nothing is written
/// to disk.
#[allow(clippy::too_many_arguments)]
pub fn syncback_new_subtree(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    parent: Ref,
    directory: &Path,
    existing: &HashSet<String>,
    replacement: Option<Ref>,
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
    let required_number = next_child_number(old_tree, parent_instance, &child.name, replacement)?;
    let stem = free_child_stem(child, middleware, existing, required_number)?;
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
        // Shared LFS rules have their own validated project/source-directory scope.
        if fs_snapshot.is_shared_attributes_path(path) {
            continue;
        }
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

/// Uses the same logical-name groups and physical ordinals as directory reload.
/// Counting siblings would backfill sparse exports, while reading physical names
/// alone would mistake an unrelated natural `Twin.__100` for the hundredth Twin.
fn next_child_number(
    old_tree: &RojoTree,
    parent: InstanceWithMeta<'_>,
    logical_name: &str,
    replacement: Option<Ref>,
) -> anyhow::Result<u64> {
    let key = logical_name.to_lowercase();
    let ordinal = |sibling: InstanceWithMeta<'_>| {
        sibling
            .metadata()
            .relevant_paths
            .first()
            .and_then(|path| path.file_name())
            .map(|file_name| {
                sibling_number(
                    &file_name.to_string_lossy(),
                    sibling.name(),
                    &sibling.class_name(),
                )
            })
            .unwrap_or(1)
    };
    if let Some(old) = replacement.and_then(|id| old_tree.get_instance(id)) {
        if old.parent() == parent.id() && old.name().to_lowercase() == key {
            return Ok(ordinal(old));
        }
    }
    let mut maximum = 0;
    for id in parent.children() {
        let sibling = old_tree
            .get_instance(*id)
            .context("A sibling is no longer known")?;
        if sibling.name().to_lowercase() == key {
            maximum = maximum.max(ordinal(sibling));
        }
    }
    maximum
        .checked_add(1)
        .with_context(|| format!("Sibling ordinals are exhausted for instance {logical_name:?}"))
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

    fn sibling_tree(siblings: &[(&str, &str, Option<&str>)]) -> RojoTree {
        RojoTree::new(
            InstanceSnapshot::new().children(
                siblings
                    .iter()
                    .map(|(name, class, source)| {
                        InstanceSnapshot::new()
                            .name(*name)
                            .class_name(*class)
                            .metadata(InstanceMetadata::new().relevant_paths(
                                source.iter().map(|path| PathBuf::from(*path)).collect(),
                            ))
                    })
                    .collect::<Vec<_>>(),
            ),
        )
    }

    #[test]
    fn sibling_floor_appends_after_the_maximum_actual_logical_sibling() {
        for (name, class) in [
            ("Twin", "Folder"),
            ("Twin.__2", "ModuleScript"),
            ("Odd?.__100", "Folder"),
        ] {
            let stem = super::super::file_names::sanitize_stem(name, class);
            let case_variant = name.to_lowercase();
            let first = format!("{stem}.luau");
            let third = format!("{stem}~3");
            let fourth = format!("{stem}.__4.server.luau");
            let tree = sibling_tree(&[
                (name, class, Some(&first)),
                (&case_variant, class, Some(&third)),
                (name, class, Some(&fourth)),
                ("Unrelated", "Folder", Some("Twin.__9999")),
            ]);
            assert_eq!(
                next_child_number(&tree, tree.root(), name, None).unwrap(),
                5
            );
            let sparse = sibling_tree(&[(name, class, Some(&third))]);
            assert_eq!(
                next_child_number(&sparse, sparse.root(), name, None).unwrap(),
                4
            );
        }
    }

    #[test]
    fn sibling_floor_uses_logical_names_instead_of_stripping_extensions() {
        let tree = sibling_tree(&[
            ("Twin", "ModuleScript", Some("Twin.luau")),
            ("Twin.__2", "Folder", Some("Twin.__2")),
            ("Twin.__100", "Folder", Some("Twin.__100")),
            ("Twin.luau", "Folder", Some("Twin.luau.__999")),
        ]);
        assert_eq!(
            next_child_number(&tree, tree.root(), "Twin", None).unwrap(),
            2
        );
        assert_eq!(
            next_child_number(&tree, tree.root(), "Twin.__100", None).unwrap(),
            2
        );
        assert_eq!(
            next_child_number(&tree, tree.root(), "Other", None).unwrap(),
            1
        );
        let virtual_siblings = sibling_tree(&[("Twin", "Folder", None)]);
        assert_eq!(
            next_child_number(&virtual_siblings, virtual_siblings.root(), "Twin", None).unwrap(),
            2
        );
        // Reload groups Unicode case variants with the same lowercase key too.
        let unicode = sibling_tree(&[("Tŵin", "Folder", Some("Tŵin~3"))]);
        assert_eq!(
            next_child_number(&unicode, unicode.root(), "TŴIN", None).unwrap(),
            4
        );
    }

    #[test]
    fn replacements_in_other_parents_or_name_groups_still_append() {
        let mut tree = sibling_tree(&[
            ("Twin", "ModuleScript", Some("Twin.luau")),
            ("Twin", "Folder", Some("Twin~3")),
            ("Other", "Folder", Some("Other")),
        ]);
        let other = tree.root().children()[2];
        let elsewhere = tree.insert_instance(other, InstanceSnapshot::new().name("Twin"));
        for replacement in [None, Some(other), Some(elsewhere), Some(Ref::new())] {
            assert_eq!(
                next_child_number(&tree, tree.root(), "Twin", replacement).unwrap(),
                4
            );
        }
    }

    #[test]
    fn replacement_keeps_its_original_sibling_ordinal() {
        let tree = sibling_tree(&[
            ("Twin", "Folder", Some("Twin")),
            ("Twin", "Folder", Some("Twin.__3.rbxm")),
            ("Twin", "Folder", Some("Twin.__8")),
        ]);
        for (index, expected) in [1, 3, 8].into_iter().enumerate() {
            assert_eq!(
                next_child_number(
                    &tree,
                    tree.root(),
                    "tWiN",
                    Some(tree.root().children()[index])
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn replacement_ordinals_still_obey_physical_and_sidecar_reservations() {
        let dom = WeakDom::new(InstanceBuilder::new("ModuleScript").with_name("Twin"));
        for number in [1, 3] {
            let stem = if number == 1 {
                "Twin".to_owned()
            } else {
                format!("Twin.__{number}")
            };
            let source = format!("{stem}.luau");
            let tree = sibling_tree(&[("Twin", "ModuleScript", Some(&source))]);
            let required =
                next_child_number(&tree, tree.root(), "Twin", Some(tree.root().children()[0]))
                    .unwrap();
            for reserved in [
                source.to_lowercase(),
                format!("{}.meta.json", stem.to_lowercase()),
                format!("{}.meta.jsonc", stem.to_lowercase()),
            ] {
                let existing = HashSet::from([reserved]);
                let actual = free_child_stem(
                    dom.root(),
                    directory_middleware_for_instance(dom.root()),
                    &existing,
                    required,
                )
                .unwrap();
                assert_eq!(actual, format!("Twin.__{}", number + 1));
            }
        }
    }

    #[test]
    fn sibling_floor_reports_exhaustion_without_overflow() {
        for marker in ["~", ".__"] {
            let last = format!("Twin{marker}{}", u64::MAX);
            let tree = sibling_tree(&[("Twin", "Folder", Some(&last))]);
            let error = next_child_number(&tree, tree.root(), "Twin", None).unwrap_err();
            assert!(error.to_string().contains("Sibling ordinals are exhausted"));
        }
    }

    #[test]
    fn incremental_additions_skip_pending_paths_without_a_parent_snapshot_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("src");
        std::fs::create_dir(&directory).unwrap();
        let project = temp.path().join("default.project.json");
        std::fs::write(&project, r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"},"syncbackRules":{"instancesAsDirectories":true}}"#).unwrap();
        for number in [1, 3] {
            let stem = if number == 1 {
                "Twin".to_owned()
            } else {
                format!("Twin~{number}")
            };
            std::fs::write(
                directory.join(format!("{stem}.luau")),
                format!("return {number}"),
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{stem}.meta.json")),
                r#"{"name":"Twin"}"#,
            )
            .unwrap();
        }
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let session = crate::serve_session::ServeSession::new(vfs, &project).unwrap();
        let tree = session.tree();
        let mut existing: HashSet<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_lowercase())
            .collect();
        // A pending sidecar reserves ordinal 4 even though no logical sibling
        // has reached the parent snapshot yet. JSON and JSONC both count.
        for extension in ["json", "jsonc"] {
            existing.insert(format!("twin.__4.meta.{extension}"));
        }
        for number in 5..=7 {
            let model = WeakDom::new(
                InstanceBuilder::new("Folder").with_child(
                    InstanceBuilder::new("ModuleScript")
                        .with_name("Twin")
                        .with_property("Source", format!("return {number}")),
                ),
            );
            let created = syncback_new_subtree(
                session.vfs(),
                &tree,
                session.root_project(),
                tree.get_root_id(),
                &directory,
                &existing,
                None,
                model,
            )
            .unwrap();
            assert_eq!(
                created.root_path,
                directory.join(format!("Twin.__{number}.luau"))
            );
            created
                .fs_snapshot
                .write_to_vfs(temp.path(), session.vfs())
                .unwrap();
            existing.insert(
                created
                    .root_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_lowercase(),
            );
        }
        assert_eq!(tree.root().children().len(), 2);
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let reloaded = crate::serve_session::ServeSession::new(vfs, &project).unwrap();
        let tree = reloaded.tree();
        let sources: Vec<_> = tree
            .root()
            .children()
            .iter()
            .map(|id| tree.get_instance(*id).unwrap().properties()[&ustr("Source")].clone())
            .collect();
        assert_eq!(
            sources,
            [1, 3, 5, 6, 7].map(|number| Variant::String(format!("return {number}")))
        );
    }

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
