use std::path::Path;

use anyhow::Context;
use memofs::{IoResultExt, Vfs};

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

use super::meta_file::AdjacentMetadata;

#[profiling::function]
pub fn snapshot_rbxm(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let temp_tree = rbx_binary::from_reader(vfs.read(path)?.as_slice())
        .with_context(|| format!("Malformed rbxm file: {}", path.display()))?;

    let root_instance = temp_tree.root();
    let children = root_instance.children();

    if children.len() == 1 {
        let child = children[0];
        let mut snapshot = InstanceSnapshot::from_tree(temp_tree, child)
            .name(name)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(path)
                    .relevant_paths(vec![vfs.canonicalize(path)?])
                    .context(context),
            );

        AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;

        Ok(Some(snapshot))
    } else {
        anyhow::bail!(
            "Rojo currently only supports model files with one top-level instance.\n\n \
             Check the model file at path {}",
            path.display()
        );
    }
}

pub fn syncback_rbxm<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let inst = snapshot.new_inst();

    // Long-term, we probably want to have some logic for if this contains a
    // script. That's a future endeavor though.
    let mut serialized = Vec::new();
    rbx_binary::to_writer(&mut serialized, snapshot.new_tree(), &[inst.referent()])
        .context("failed to serialize new rbxm")?;

    let mut fs_snapshot = FsSnapshot::new().with_added_file(&snapshot.path, serialized);
    syncback_model_name(snapshot, &mut fs_snapshot)?;
    if inst.class == "Terrain" {
        fs_snapshot.plan_terrain_lfs(snapshot.project(), snapshot.vfs(), &snapshot.path)?;
    }

    Ok(SyncbackReturn {
        fs_snapshot,
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

/// Keeps the adjacent metadata of a model file in step with its root's name.
/// The root is named after the file when it is read, so a name the file system
/// can't hold, or one shared with a sibling, is stored in the metadata. The
/// model file holds the root's properties and attributes, so the metadata
/// stops setting them. Root identity and ignore behavior also survive when the
/// old representation was removed and no destination sidecar exists yet.
pub(super) fn syncback_model_name(
    snapshot: &SyncbackSnapshot,
    fs_snapshot: &mut FsSnapshot,
) -> anyhow::Result<()> {
    let directory = snapshot
        .path
        .parent()
        .context("model files should have a parent directory")?;
    let stem = snapshot.file_stem();
    let json_path = directory.join(format!("{stem}.meta.json"));
    let jsonc_path = directory.join(format!("{stem}.meta.jsonc"));
    let json = snapshot.vfs().read(&json_path).with_not_found()?;
    let jsonc = snapshot.vfs().read(&jsonc_path).with_not_found()?;

    // Both files are applied when reading, so rewriting only one of them can't
    // clear a name the other still sets.
    let (meta_path, existing) = match (json, jsonc) {
        (Some(_), Some(_)) => anyhow::bail!(
            "Both {} and {} hold this model's metadata, so it can't be safely rewritten",
            json_path.display(),
            jsonc_path.display()
        ),
        (None, Some(existing)) => (jsonc_path, Some(existing)),
        (existing, None) => (json_path, existing),
    };
    let existing = existing.as_deref().map(Vec::as_slice);
    let (_, attributes) = super::meta_file::serialize_values(
        &snapshot.new_inst().class,
        snapshot
            .new_inst()
            .properties
            .iter()
            .filter(|(_, value)| matches!(value, rbx_dom_weak::types::Variant::Attributes(_)))
            .map(|(key, value)| (*key, value)),
    );
    let updated = AdjacentMetadata::for_model_root(
        existing,
        &meta_path,
        snapshot.name_override(),
        snapshot.old_metadata(),
        &attributes,
    )?;
    let meta_path = crate::syncback::reused_model_metadata_path(snapshot, &meta_path)?;
    match updated {
        // A fresh layout removes the old model and its sidecar, even when
        // the destination reuses that sidecar without changing its bytes.
        Some(contents)
            if snapshot.metadata_source.is_some() || existing != Some(contents.as_slice()) =>
        {
            fs_snapshot.add_file(meta_path, contents)
        }
        Some(_) => {}
        None if existing.is_some() => fs_snapshot.remove_file(meta_path),
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn fresh_model_relayout_preserves_root_metadata_without_stale_contents() {
        use crate::{serve_session::ServeSession, syncback::syncback_loop, REF_ID_ATTRIBUTE_NAME};
        use rbx_dom_weak::{
            types::{Attributes, Variant},
            ustr, InstanceBuilder, WeakDom,
        };

        for (format, same_path, directories) in ["rbxm", "rbxmx"]
            .into_iter()
            .flat_map(|format| [false, true].map(move |same_path| (format, same_path)))
            .flat_map(|(format, same_path)| {
                [false, true].map(move |directories| (format, same_path, directories))
            })
        {
            for extension in ["json", "jsonc"] {
                for bits in 0..8 {
                    for ignore in [false, true] {
                        for attribute_id in [false, true] {
                            let temp = tempfile::tempdir().unwrap();
                            let vfs = Vfs::new_default().unwrap();
                            vfs.set_watch_enabled(false);
                            let project_path = temp.path().join("default.project.json");
                            vfs.write(&project_path, format!(r#"{{"name":"Test","tree":{{"$path":"src"}},"syncbackRules":{{"instancesAsDirectories":{directories},"modelTrees":[{{"pattern":"Root","format":"{format}"}}]}}}}"#)).unwrap();
                            let source = temp.path().join(if same_path {
                                format!("src/Root.{format}")
                            } else {
                                "src/Root".into()
                            });
                            vfs.create_dir_all(&source).unwrap();
                            vfs.write(
                                source.join(if same_path {
                                    "init.luau"
                                } else {
                                    "StaleScript.luau"
                                }),
                                "return 'stale'",
                            )
                            .unwrap();
                            let mut meta = serde_json::json!({"properties":{"Archivable":false},"attributes":{"Stale":1},"children":[{"className":"Folder","name":"Stale"}]});
                            if same_path {
                                meta["name"] = "Root".into();
                            }
                            if bits & 1 != 0 {
                                meta["id"] = "owner".into();
                            }
                            if bits & 2 != 0 {
                                meta["$schema"] = "schema.json".into();
                            }
                            if bits & 4 != 0 {
                                meta["ignoreUnknownInstances"] = ignore.into();
                            }
                            vfs.write(
                                source.join(format!("init.meta.{extension}")),
                                serde_json::to_vec(&meta).unwrap(),
                            )
                            .unwrap();
                            let session = ServeSession::new(vfs, &project_path).unwrap();
                            let mut root =
                                InstanceBuilder::new("Folder").with_name("Root").with_child(
                                    InstanceBuilder::new("Folder").with_name("Replacement"),
                                );
                            if attribute_id {
                                let mut attrs = Attributes::new();
                                attrs.insert(
                                    REF_ID_ATTRIBUTE_NAME.into(),
                                    Variant::String("new-owner".into()),
                                );
                                root = root.with_property("Attributes", attrs);
                            }
                            let incoming =
                                WeakDom::new(InstanceBuilder::new("Folder").with_child(root));
                            let output = syncback_loop(
                                session.vfs(),
                                &mut session.tree(),
                                incoming,
                                session.root_project(),
                            )
                            .unwrap();
                            output.write_to_vfs(temp.path(), session.vfs()).unwrap();
                            let model_path = temp.path().join(format!("src/Root.{format}"));
                            let rebuilt = super::super::snapshot_from_vfs(
                                &InstanceContext::default(),
                                session.vfs(),
                                &model_path,
                            )
                            .unwrap()
                            .unwrap();
                            assert_eq!(rebuilt.metadata.specified_id.as_ref().map(ToString::to_string), (bits & 1 != 0 && !attribute_id).then(|| "owner".to_owned()), "{format} {extension} bits {bits} ignore {ignore} attribute {attribute_id} same path {same_path}, writes {:?}, removals {:?}", output.added_files(), output.removed_paths());
                            assert_eq!(
                                rebuilt.metadata.schema.as_deref(),
                                (bits & 2 != 0).then_some("schema.json")
                            );
                            assert_eq!(
                                rebuilt.metadata.ignore_unknown_instances,
                                bits & 4 != 0 && ignore
                            );
                            assert_eq!(rebuilt.class_name, "Folder");
                            assert_eq!(rebuilt.children.len(), 1);
                            assert_eq!(rebuilt.children[0].name, "Replacement");
                            assert!(!rebuilt.properties.contains_key(&ustr("Source")));
                            assert_ne!(
                                rebuilt.properties.get(&ustr("Archivable")),
                                Some(&Variant::Bool(false))
                            );
                            if let Some(Variant::Attributes(attrs)) =
                                rebuilt.properties.get(&ustr("Attributes"))
                            {
                                assert!(attrs.get("Stale").is_none());
                                let id = match attrs.get(REF_ID_ATTRIBUTE_NAME) {
                                    Some(Variant::String(value)) => Some(value.as_str()),
                                    Some(Variant::BinaryString(value)) => {
                                        std::str::from_utf8(value.as_ref()).ok()
                                    }
                                    _ => None,
                                };
                                assert_eq!(id, attribute_id.then_some("new-owner"));
                            } else {
                                assert!(!attribute_id);
                            }
                            if let Ok(bytes) =
                                session.vfs().read(temp.path().join("src/Root.meta.json"))
                            {
                                let meta: serde_json::Value =
                                    serde_json::from_slice(&bytes).unwrap();
                                assert!(meta.get("properties").is_none());
                                assert!(meta.get("attributes").is_none());
                                assert!(meta.get("children").is_none());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fresh_model_relayout_restages_unchanged_adjacent_identity() {
        use crate::{serve_session::ServeSession, syncback::syncback_loop};
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for format in ["rbxm", "rbxmx"] {
            let temp = tempfile::tempdir().unwrap();
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            let project_path = temp.path().join("default.project.json");
            vfs.write(&project_path, format!(r#"{{"name":"Test","tree":{{"$path":"src"}},"syncbackRules":{{"instancesAsDirectories":true,"modelTrees":[{{"pattern":"Root","format":"{format}"}}]}}}}"#)).unwrap();
            vfs.create_dir_all(temp.path().join("src")).unwrap();
            vfs.write(temp.path().join("src/Root.luau"), "return 1")
                .unwrap();
            let sidecar = temp.path().join("src/Root.meta.json");
            let identity = crate::json::to_vec_pretty(&serde_json::json!({"id":"owner"})).unwrap();
            vfs.write(&sidecar, &identity).unwrap();
            let session = ServeSession::new(vfs, &project_path).unwrap();
            let incoming = WeakDom::new(
                InstanceBuilder::new("Folder").with_child(
                    InstanceBuilder::new("ModuleScript")
                        .with_name("Root")
                        .with_property("Source", "return 2"),
                ),
            );
            let output = syncback_loop(
                session.vfs(),
                &mut session.tree(),
                incoming,
                session.root_project(),
            )
            .unwrap();
            output.write_to_vfs(temp.path(), session.vfs()).unwrap();
            assert_eq!(session.vfs().read(&sidecar).unwrap().as_slice(), identity);
            let rebuilt = super::super::snapshot_from_vfs(
                &InstanceContext::default(),
                session.vfs(),
                &temp.path().join(format!("src/Root.{format}")),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                rebuilt
                    .metadata
                    .specified_id
                    .as_ref()
                    .map(ToString::to_string),
                Some("owner".into())
            );
        }
    }

    #[test]
    fn model_sidecars_keep_scoped_jsonc_ownership_and_refuse_dual_files() {
        use crate::{
            serve_session::ServeSession,
            syncback::{syncback_model_file, validate_write_targets},
        };
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for format in ["rbxm", "rbxmx", "model.json", "model.jsonc"] {
            for (dual, ignored) in [(false, false), (false, true), (true, false)] {
                let temp = tempfile::tempdir().unwrap();
                let vfs = Vfs::new_default().unwrap();
                vfs.set_watch_enabled(false);
                let project_path = temp.path().join("default.project.json");
                let mut project = serde_json::json!({"name":"Test","tree":{"$path":"src"}});
                if ignored {
                    project["syncbackRules"] =
                        serde_json::json!({"ignorePaths":["**/*.meta.jsonc"]});
                }
                vfs.write(&project_path, serde_json::to_vec(&project).unwrap())
                    .unwrap();
                vfs.create_dir_all(temp.path().join("src")).unwrap();
                let path = temp.path().join(format!("src/Root.{format}"));
                let dom = WeakDom::new(
                    InstanceBuilder::new("DataModel")
                        .with_child(InstanceBuilder::new("Folder").with_name("Root")),
                );
                let mut bytes = Vec::new();
                match format {
                    "rbxm" => {
                        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap()
                    }
                    "rbxmx" => {
                        rbx_xml::to_writer_default(&mut bytes, &dom, dom.root().children()).unwrap()
                    }
                    _ => bytes.extend_from_slice(br#"{"className":"Folder"}"#),
                }
                vfs.write(&path, bytes).unwrap();
                let meta_path = temp.path().join("src/Root.meta.jsonc");
                let original = r#"{"id":"owner","$schema":"schema.json","ignoreUnknownInstances":true,"attributes":{"Stale":1}}"#;
                vfs.write(&meta_path, original).unwrap();
                if dual {
                    vfs.write(meta_path.with_extension("json"), "{}").unwrap();
                }
                let session = ServeSession::new(vfs, &project_path).unwrap();
                let meta_path = session.vfs().canonicalize(&meta_path).unwrap();
                let root = session.tree().root().children()[0];
                let incoming = WeakDom::new(
                    InstanceBuilder::new("Folder")
                        .with_name("Root")
                        .with_child(InstanceBuilder::new("Folder").with_name("Replacement")),
                );
                let result = syncback_model_file(
                    session.vfs(),
                    &session.tree(),
                    session.root_project(),
                    root,
                    incoming,
                );
                assert_eq!(
                    session.vfs().read_to_string(&meta_path).unwrap().as_str(),
                    original
                );
                if dual {
                    assert!(result.err().unwrap().to_string().contains("Both"));
                    continue;
                }
                if ignored && format.starts_with("model.") {
                    let error = result.err().unwrap();
                    assert!(error
                        .downcast_ref::<crate::syncback::IgnoreConflict>()
                        .is_some());
                    assert!(error.to_string().contains("excluded"));
                    continue;
                }
                let output = result.unwrap();
                assert!(!output
                    .added_files()
                    .contains(&meta_path.with_extension("json").as_path()));
                assert!(output.removed_paths().is_empty());
                if ignored {
                    assert!(validate_write_targets(session.root_project(), &output).is_err());
                    continue;
                }
                validate_write_targets(session.root_project(), &output).unwrap();
                assert!(
                    output.added_files().contains(&meta_path.as_path()),
                    "{format}, writes {:?}, original metadata {}",
                    output.added_files(),
                    session.vfs().read_to_string(&meta_path).unwrap()
                );
                output.write_to_vfs(temp.path(), session.vfs()).unwrap();
                let rebuilt = crate::snapshot_middleware::snapshot_from_vfs(
                    &InstanceContext::default(),
                    session.vfs(),
                    &path,
                )
                .unwrap()
                .unwrap();
                assert_eq!(rebuilt.metadata.specified_id.unwrap().to_string(), "owner");
                assert_eq!(rebuilt.metadata.schema.as_deref(), Some("schema.json"));
                assert!(rebuilt.metadata.ignore_unknown_instances);
                assert_eq!(rebuilt.children.len(), 1);
                assert_eq!(rebuilt.children[0].name, "Replacement");
            }
        }
    }

    #[test]
    fn only_terrain_roots_plan_lfs_and_staging_does_not_write() {
        use crate::{project::Project, snapshot::RojoTree, syncback::syncback_model_file};
        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        for class in ["Terrain", "Folder"] {
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot("/project", VfsSnapshot::empty_dir())
                .unwrap();
            backend
                .load_snapshot(
                    "/project/default.project.json",
                    VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
                )
                .unwrap();
            let vfs = Vfs::new(backend);
            let project =
                Project::load_exact(&vfs, Path::new("/project/default.project.json"), None)
                    .unwrap();
            let path = Path::new("/project/Root.rbxm");
            let old = RojoTree::new(
                InstanceSnapshot::new()
                    .name("Root")
                    .class_name(class)
                    .metadata(
                        InstanceMetadata::new()
                            .instigating_source(path)
                            .middleware(super::super::Middleware::Rbxm),
                    ),
            );
            let dom = WeakDom::new(InstanceBuilder::new(class).with_name("Root"));
            let staged = syncback_model_file(&vfs, &old, &project, old.get_root_id(), dom).unwrap();
            assert_eq!(
                staged
                    .added_files()
                    .contains(&Path::new("/project/.gitattributes")),
                class == "Terrain"
            );
            assert!(vfs.read("/project/.gitattributes").is_err());
            assert!(vfs.read(path).is_err());
        }
    }

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.rbxm",
            VfsSnapshot::file(include_bytes!("../../assets/test-folder.rbxm").to_vec()),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_rbxm(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.rbxm"),
            "foo",
        )
        .unwrap()
        .unwrap();

        assert_eq!(instance_snapshot.name, "foo");
        assert_eq!(instance_snapshot.class_name, "Folder");
        assert_eq!(instance_snapshot.children, Vec::new());

        // We intentionally don't assert on properties. rbx_binary does not
        // distinguish between String and BinaryString. The sample model was
        // created by Roblox Studio and has an empty BinaryString "Tags"
        // property that currently deserializes incorrectly.
        // See: https://github.com/Roblox/rbx-dom/issues/49
    }
}
