use std::{borrow::Cow, path::Path, str};

use anyhow::Context;
use indexmap::IndexMap;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    HashMapExt as _, Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};

use crate::{
    json,
    resolution::{normalize_property_names, UnresolvedValue},
    snapshot::{InstanceContext, InstanceSnapshot},
    snapshot_middleware::meta_file::{
        metadata_id, serialize_values, write_metadata, AdjacentMetadata,
    },
    syncback::{
        filter_json_properties_preallocated, reused_model_metadata_path, validate_write_targets,
        FsSnapshot, IgnoreConflict, SyncbackReturn, SyncbackSnapshot,
    },
    RojoRef,
};

pub fn snapshot_json_model(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let contents = vfs.read(path)?;
    let contents_str = str::from_utf8(&contents)
        .with_context(|| format!("File was not valid UTF-8: {}", path.display()))?;

    if contents_str.trim().is_empty() {
        return Ok(None);
    }

    let mut instance: JsonModel = json::from_str_with_context(contents_str, || {
        format!("File is not a valid JSON model: {}", path.display())
    })?;

    if let Some(top_level_name) = &instance.name {
        let new_name = format!("{}.model.json", top_level_name);

        log::warn!(
            "Model at path {} had a top-level Name field. \
            This field has been ignored since Rojo 6.0.\n\
            Consider removing this field and renaming the file to {}.",
            new_name,
            path.display()
        );
    }

    instance.name = Some(name.to_owned());

    let id = instance.id.take().map(RojoRef::new);
    let schema = instance.schema.take();

    let mut snapshot = instance
        .into_snapshot()
        .with_context(|| format!("Could not load JSON model: {}", path.display()))?;

    snapshot.metadata = snapshot
        .metadata
        .instigating_source(path)
        .relevant_paths(vec![vfs.canonicalize(path)?])
        .context(context)
        .specified_id(id)
        .schema(schema);

    AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;

    Ok(Some(snapshot))
}

pub fn syncback_json_model<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let mut property_buffer = Vec::with_capacity(snapshot.new_inst().properties.len());

    let mut model = json_model_from_pair(snapshot, &mut property_buffer, snapshot.new);
    // We don't need the name on the root, but we do for children.
    model.name = None;

    if let Some(metadata) = snapshot.old_metadata() {
        model.id = metadata_id(metadata.specified_id.as_ref(), &model.attributes);
        model.schema = metadata.schema.clone();
    }

    let mut fs_snapshot = FsSnapshot::new();
    if let Some(mut metadata) =
        AdjacentMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?
    {
        // JSON models support root identity inline, but ignore behavior and name
        // overrides belong in the sidecar. Never write identity in both places.
        metadata.id = None;
        metadata.schema = None;
        metadata.properties.clear();
        metadata.attributes.clear();
        let parent = snapshot
            .path
            .parent()
            .context("JSON models should have a parent directory")?;
        let meta_path = parent.join(format!("{}.meta.json", snapshot.file_stem()));
        write_metadata(
            snapshot.vfs(),
            &mut fs_snapshot,
            &meta_path,
            &metadata,
            metadata.is_empty(),
        )?;
    }
    // Do not rewrite equivalent metadata merely to normalize formatting. In
    // particular, an ignored sidecar can stay untouched when no values move.
    // Fresh layouts must still restage it because shared cleanup removes it.
    if snapshot.metadata_source.is_none()
        && fs_snapshot
            .file_changes()
            .into_iter()
            .all(|(path, contents)| {
                let Some(contents) = contents else {
                    return false;
                };
                let Ok(existing) = snapshot.vfs().read(path) else {
                    return false;
                };
                match (
                    json::from_slice::<serde_json::Value>(&existing),
                    json::from_slice::<serde_json::Value>(contents),
                ) {
                    (Ok(existing), Ok(updated)) => existing == updated,
                    _ => false,
                }
            })
    {
        fs_snapshot = FsSnapshot::new();
    }
    let metadata_paths: Vec<_> = fs_snapshot
        .added_files()
        .into_iter()
        .map(Path::to_owned)
        .collect();
    for path in metadata_paths {
        let tracked = reused_model_metadata_path(snapshot, &path)?;
        fs_snapshot.retarget_added_file(&path, &tracked);
    }
    // Identity moves inline only if its required sidecar rewrite can be
    // persisted too. Full syncback would otherwise filter the sidecar later,
    // leaving duplicate IDs or schemas that make the next build fail.
    validate_write_targets(snapshot.project(), &fs_snapshot)
        .map_err(|error| IgnoreConflict(error.to_string()))?;
    fs_snapshot.add_file(
        &snapshot.path,
        crate::json::to_vec_pretty(&model).context("failed to serialize new JSON Model")?,
    );

    Ok(SyncbackReturn {
        fs_snapshot,
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

fn json_model_from_pair<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
    prop_buffer: &mut Vec<(Ustr, &'sync Variant)>,
    new: Ref,
) -> JsonModel {
    let new_inst = snapshot
        .get_new_instance(new)
        .expect("all new referents passed to json_model_from_pair should exist");

    filter_json_properties_preallocated(snapshot.project(), new_inst, prop_buffer);
    let (properties, attributes) = serialize_values(&new_inst.class, prop_buffer.drain(..));

    let mut children = Vec::with_capacity(new_inst.children().len());

    for new_child_ref in new_inst.children() {
        children.push(json_model_from_pair(snapshot, prop_buffer, *new_child_ref))
    }

    JsonModel {
        name: Some(new_inst.name.clone()),
        class_name: new_inst.class,
        children,
        properties,
        attributes,
        id: None,
        schema: None,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonModel {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String>,

    #[serde(alias = "Name", skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    #[serde(alias = "ClassName")]
    class_name: Ustr,

    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,

    #[serde(
        alias = "Children",
        default = "Vec::new",
        skip_serializing_if = "Vec::is_empty"
    )]
    children: Vec<JsonModel>,

    #[serde(
        alias = "Properties",
        default,
        skip_serializing_if = "IndexMap::is_empty"
    )]
    properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(default = "IndexMap::new", skip_serializing_if = "IndexMap::is_empty")]
    attributes: IndexMap<String, UnresolvedValue>,
}

impl JsonModel {
    fn into_snapshot(self) -> anyhow::Result<InstanceSnapshot> {
        let name = self.name.unwrap_or_else(|| self.class_name.to_owned());
        let class_name = self.class_name;

        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_snapshot()?);
        }

        let mut properties = UstrMap::with_capacity(self.properties.len());
        for (key, unresolved) in normalize_property_names(&class_name, self.properties) {
            let value = unresolved.resolve(&class_name, &key)?;
            properties.insert(key, value);
        }

        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();

            for (key, unresolved) in self.attributes {
                let value = unresolved.resolve_unambiguous()?;
                attributes.insert(key, value);
            }

            properties.insert("Attributes".into(), attributes.into());
        }

        Ok(InstanceSnapshot {
            snapshot_id: Ref::none(),
            metadata: Default::default(),
            name: Cow::Owned(name),
            class_name,
            properties,
            children,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn removed_json_models_clean_only_exact_tracked_sidecars() {
        use crate::{serve_session::ServeSession, syncback::syncback_loop};
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for extension in ["json", "jsonc"] {
            for target in ["rbxm", "rbxmx"] {
                // This backend is case-sensitive even on a case-insensitive host.
                let vfs = Vfs::new(InMemoryFs::new());
                vfs.create_dir_all("/project/src").unwrap();
                let project_path = Path::new("/project/default.project.json");
                vfs.write(
                    project_path,
                    serde_json::json!({
                        "name":"Review", "tree":{"$path":"src"},
                        "syncbackRules":{"instancesAsDirectories":false,
                            "modelTrees":[{"pattern":"Renamed", "format":target}]}
                    })
                    .to_string(),
                )
                .unwrap();
                let source = format!("/project/src/ROOT.model.{extension}");
                vfs.write(&source, r#"{"className":"Folder"}"#).unwrap();
                let tracked = format!("/project/src/ROOT.meta.{extension}");
                let untracked = format!("/project/src/root.meta.{extension}");
                vfs.write(&tracked, r#"{"name":"Renamed","id":"owner","$schema":"schema.json","ignoreUnknownInstances":true}"#).unwrap();
                let untouched = r#"{"id":"unrelated"}"#;
                vfs.write(&untracked, untouched).unwrap();
                let session = ServeSession::new(vfs, project_path).unwrap();
                let incoming = WeakDom::new(
                    InstanceBuilder::new("Folder").with_child(
                        InstanceBuilder::new("Folder")
                            .with_name("Renamed")
                            .with_child(InstanceBuilder::new("Folder").with_name("Added")),
                    ),
                );
                let output = syncback_loop(
                    session.vfs(),
                    &mut session.tree(),
                    incoming,
                    session.root_project(),
                )
                .unwrap();
                assert!(output.removed_files().contains(&Path::new(&tracked)));
                assert!(!output.removed_files().contains(&Path::new(&untracked)));
                output.write_to_vfs("/project", session.vfs()).unwrap();
                assert!(session.vfs().metadata(&source).is_err());
                assert!(session.vfs().metadata(&tracked).is_err());
                assert_eq!(
                    session.vfs().read_to_string(&untracked).unwrap().as_str(),
                    untouched
                );
                let rebuilt = super::super::snapshot_from_vfs(
                    &InstanceContext::default(),
                    session.vfs(),
                    &Path::new("/project/src").join(format!("Renamed.{target}")),
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
                assert_eq!(rebuilt.metadata.schema.as_deref(), Some("schema.json"));
                assert!(rebuilt.metadata.ignore_unknown_instances);
                assert_eq!(rebuilt.children[0].name, "Added");
            }
        }
    }

    #[test]
    fn json_model_rewrite_preserves_supported_root_metadata_once() {
        use crate::{serve_session::ServeSession, syncback::syncback_model_file};
        use rbx_dom_weak::{types::Attributes, InstanceBuilder, WeakDom};

        for extension in ["json", "jsonc"] {
            for meta_extension in ["json", "jsonc"] {
                for bits in 0..8 {
                    for (ignore, attribute_id) in [false, true]
                        .into_iter()
                        .flat_map(|ignore| [false, true].map(|attribute_id| (ignore, attribute_id)))
                    {
                        let temp = tempfile::tempdir().unwrap();
                        let vfs = Vfs::new_default().unwrap();
                        vfs.set_watch_enabled(false);
                        let project_path = temp.path().join("default.project.json");
                        vfs.write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#)
                            .unwrap();
                        vfs.create_dir_all(temp.path().join("src")).unwrap();
                        let path = temp.path().join(format!("src/Root.model.{extension}"));
                        let mut model = serde_json::json!({"className":"Folder","properties":{"Archivable":false},"attributes":{"Stale":1}});
                        if bits & 1 != 0 {
                            model["id"] = "owner".into();
                        }
                        if bits & 2 != 0 {
                            model["$schema"] = "schema.json".into();
                        }
                        vfs.write(&path, serde_json::to_vec(&model).unwrap())
                            .unwrap();
                        let sidecar = temp.path().join(format!("src/Root.meta.{meta_extension}"));
                        if bits & 4 != 0 {
                            vfs.write(
                                &sidecar,
                                format!(r#"{{"ignoreUnknownInstances":{ignore}}}"#),
                            )
                            .unwrap();
                        }
                        let session = ServeSession::new(vfs, &project_path).unwrap();
                        let root = session.tree().root().children()[0];
                        let mut incoming_root = InstanceBuilder::new("Folder")
                            .with_name("Root")
                            .with_child(InstanceBuilder::new("Folder").with_name("Replacement"));
                        if attribute_id {
                            let mut attributes = Attributes::new();
                            attributes.insert(
                                crate::REF_ID_ATTRIBUTE_NAME.into(),
                                Variant::String("new-owner".into()),
                            );
                            incoming_root = incoming_root.with_property("Attributes", attributes);
                        }
                        let incoming = WeakDom::new(incoming_root);
                        let output = syncback_model_file(
                            session.vfs(),
                            &session.tree(),
                            session.root_project(),
                            root,
                            incoming,
                        )
                        .unwrap();
                        output.write_to_vfs(temp.path(), session.vfs()).unwrap();
                        let rebuilt = crate::snapshot_middleware::snapshot_from_vfs(
                            &crate::snapshot::InstanceContext::default(),
                            session.vfs(),
                            &path,
                        )
                        .unwrap()
                        .unwrap();
                        assert_eq!(
                            rebuilt
                                .metadata
                                .specified_id
                                .as_ref()
                                .map(ToString::to_string),
                            (bits & 1 != 0 && !attribute_id).then(|| "owner".to_owned()),
                            "{extension} {meta_extension} bits {bits} ignore {ignore}"
                        );
                        assert_eq!(
                            rebuilt.metadata.schema.as_deref(),
                            (bits & 2 != 0).then_some("schema.json")
                        );
                        assert_eq!(
                            rebuilt.metadata.ignore_unknown_instances,
                            bits & 4 != 0 && ignore
                        );
                        assert!(!rebuilt
                            .properties
                            .contains_key(&rbx_dom_weak::ustr("Archivable")));
                        if let Some(Variant::Attributes(attributes)) =
                            rebuilt.properties.get(&rbx_dom_weak::ustr("Attributes"))
                        {
                            assert!(attributes.get("Stale").is_none());
                            assert_eq!(
                                attributes.get(crate::REF_ID_ATTRIBUTE_NAME),
                                Some(&Variant::String("new-owner".into()))
                            );
                            assert!(attribute_id);
                        } else {
                            assert!(!attribute_id);
                        }
                        let model: serde_json::Value =
                            serde_json::from_slice(&session.vfs().read(&path).unwrap()).unwrap();
                        assert!(model.get("ignoreUnknownInstances").is_none());
                        for sidecar in [sidecar, temp.path().join("src/Root.meta.json")] {
                            if let Ok(bytes) = session.vfs().read(sidecar) {
                                let meta: serde_json::Value =
                                    crate::json::from_slice(&bytes).unwrap();
                                assert!(meta.get("id").is_none());
                                assert!(meta.get("$schema").is_none());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn inline_root_identity_and_schema_cannot_be_duplicated_by_sidecars() {
        for extension in ["json", "jsonc"] {
            for field in ["id", "$schema"] {
                let mut imfs = InMemoryFs::new();
                let mut model = serde_json::json!({"className":"Folder"});
                model[field] = "identity".into();
                imfs.load_snapshot(
                    "/Root.model.json",
                    VfsSnapshot::file(serde_json::to_vec(&model).unwrap()),
                )
                .unwrap();
                let mut sidecar = serde_json::json!({});
                sidecar[field] = "identity".into();
                imfs.load_snapshot(
                    format!("/Root.meta.{extension}"),
                    VfsSnapshot::file(serde_json::to_vec(&sidecar).unwrap()),
                )
                .unwrap();
                let error = snapshot_json_model(
                    &InstanceContext::default(),
                    &Vfs::new(imfs),
                    Path::new("/Root.model.json"),
                    "Root",
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains(if field == "id" {
                        "cannot specify an ID"
                    } else {
                        "cannot specify a schema"
                    }),
                    "{error:#}"
                );
            }
        }
    }

    fn without_adjacent_paths(mut snapshot: InstanceSnapshot) -> InstanceSnapshot {
        assert_eq!(
            snapshot.metadata.relevant_paths,
            [
                Path::new("/foo.model.json"),
                Path::new("/foo.meta.json"),
                Path::new("/foo.meta.jsonc"),
            ]
        );
        // Assert the new dependencies directly while keeping the existing snapshots
        // as regression coverage for all unrelated model content and metadata.
        snapshot.metadata.relevant_paths.truncate(1);
        snapshot
    }

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "className": "IntValue",
                      "properties": {
                        "Value": 5
                      },
                      "children": [
                        {
                          "name": "The Child",
                          "className": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(without_adjacent_paths(instance_snapshot));
    }

    #[test]
    fn model_from_vfs_legacy() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "ClassName": "IntValue",
                      "Properties": {
                        "Value": 5
                      },
                      "Children": [
                        {
                          "Name": "The Child",
                          "ClassName": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(without_adjacent_paths(instance_snapshot));
    }
}
