use std::{
    fs, thread,
    time::{Duration, Instant},
};

use insta::{assert_snapshot, assert_yaml_snapshot, with_settings};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, UstrMap,
};
use reqwest::StatusCode;
use tempfile::tempdir;

use crate::rojo_test::{
    internable::InternAndRedact,
    serve_util::{deserialize_msgpack, run_serve_test, serialize_msgpack, serialize_to_xml_model},
};

use librojo::web_api::{
    InstanceUpdate, SerializeResponse, SocketPacketBody, SocketPacketType, WriteAuthority,
    WriteRequest,
};

#[test]
fn interactive_scaling_saves_successive_properties_from_the_same_client_cursor() {
    use crate::rojo_test::serve_util::TestServeSession;
    use rbx_dom_weak::types::{CFrame, Matrix3, Vector3};
    let mut session = TestServeSession::with_setup("add_folder", |root| {
        fs::create_dir(root.join("src/Part")).unwrap();
        fs::write(
            root.join("src/Part/init.meta.json"),
            r#"{"className":"Part","properties":{"Transparency":0.25}}"#,
        )
        .unwrap();
    });
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let id = read
        .instances
        .values()
        .find(|instance| instance.name == "Part")
        .unwrap()
        .id;
    let cursor = read.message_cursor;
    let mut acknowledged_cursor = cursor;
    for width in [2.0, 3.0, 4.0, 5.0] {
        let size = Variant::Vector3(Vector3::new(width, 2.0, 2.0));
        let frame = Variant::CFrame(CFrame::new(
            Vector3::new(width, 0.0, 0.0),
            Matrix3::identity(),
        ));
        let response = session.post_api_write(&WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(cursor),
            authority: WriteAuthority::Filesystem,
            added: Default::default(),
            removed: Vec::new(),
            updated: vec![InstanceUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_metadata: None,
                changed_properties: UstrMap::from_iter([
                    (ustr("Size"), Some(size.clone())),
                    (ustr("CFrame"), Some(frame.clone())),
                ]),
            }],
        });
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(
            response.conflicts.updated_instances.is_empty(),
            "{:?}",
            response.conflicts
        );
        assert_eq!(response.accepted.updated_instances.len(), 1);
        assert!(response.message_cursor > acknowledged_cursor);
        acknowledged_cursor = response.message_cursor;
        let read = session.get_api_read(id).unwrap();
        assert!(read.message_cursor >= response.message_cursor);
        assert_eq!(
            read.instances[&id].properties[&ustr("Size")].as_ref(),
            &size
        );
        assert_eq!(
            read.instances[&id].properties[&ustr("CFrame")].as_ref(),
            &frame
        );
        assert_eq!(
            read.instances[&id].properties[&ustr("Transparency")].as_ref(),
            &Variant::Float32(0.25)
        );
        let metadata: serde_json::Value = serde_json::from_slice(
            &fs::read(session.path().join("src/Part/init.meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["properties"]["Size"][0].as_f64(),
            Some(f64::from(width))
        );
    }
}

#[test]
fn canonical_sound_writes_replace_authored_aliases_on_disk() {
    use crate::rojo_test::serve_util::TestServeSession;
    for storage in ["model", "directory", "inline"] {
        let mut session = TestServeSession::with_setup("add_folder", |root| {
            let source = match storage {
                "model" => root.join("src/Sound.model.json"),
                "directory" => {
                    fs::create_dir(root.join("src/Sound")).unwrap();
                    root.join("src/Sound/init.meta.json")
                }
                "inline" => {
                    fs::create_dir(root.join("src/Container")).unwrap();
                    root.join("src/Container/init.meta.json")
                }
                _ => unreachable!(),
            };
            let sound = r#"{"name":"Sound","className":"Sound","properties":{"EmitterSize":5}}"#;
            let contents = if storage == "inline" {
                format!(r#"{{"children":[{sound}]}}"#)
            } else {
                sound.to_owned()
            };
            fs::write(source, contents).unwrap();
        });
        let info = session.wait_to_come_online();
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let sound = read
            .instances
            .values()
            .find(|instance| instance.name == "Sound")
            .unwrap();
        let id = sound.id;
        assert!(!sound.properties.contains_key(&ustr("EmitterSize")));
        assert_eq!(
            sound.properties[&ustr("RollOffMinDistance")].as_ref(),
            &Variant::Float32(5.0)
        );
        let source = session.path().join(match storage {
            "model" => "src/Sound.model.json",
            "directory" => "src/Sound/init.meta.json",
            "inline" => "src/Container/init.meta.json",
            _ => unreachable!(),
        });
        for value in [10.0, 7.0] {
            let response = session.post_api_write(&WriteRequest {
                session_id: info.session_id,
                message_cursor: Some(read.message_cursor),
                authority: WriteAuthority::Filesystem,
                added: Default::default(),
                removed: Vec::new(),
                updated: vec![InstanceUpdate {
                    id,
                    changed_name: None,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_metadata: None,
                    changed_properties: UstrMap::from_iter([(
                        ustr("RollOffMinDistance"),
                        Some(Variant::Float32(value)),
                    )]),
                }],
            });
            assert!(response.errors.is_empty(), "{storage}, {response:?}");
            assert!(
                response.conflicts.updated_instances.is_empty(),
                "{storage}, {response:?}"
            );
            assert_eq!(response.accepted.updated_instances.len(), 1, "{storage}");
            let contents = fs::read_to_string(&source).unwrap();
            assert!(!contents.contains("EmitterSize"), "{storage}, {contents}");
            let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
            let properties = if storage == "inline" {
                &json["children"][0]["properties"]
            } else {
                &json["properties"]
            };
            if value == 10.0 {
                assert!(properties.get("RollOffMinDistance").is_none(), "{contents}");
            } else {
                assert_eq!(properties["RollOffMinDistance"], value, "{contents}");
            }
            let readback = session.get_api_read(id).unwrap();
            let actual = readback.instances[&id]
                .properties
                .get(&ustr("RollOffMinDistance"))
                .map(|value| value.as_ref().clone())
                .unwrap_or(Variant::Float32(10.0));
            assert_eq!(actual, Variant::Float32(value), "{storage}");
        }
    }
}

#[test]
fn text_values_sync_in_both_directions_after_filesystem_edits() {
    use crate::rojo_test::serve_util::TestServeSession;
    let mut session = TestServeSession::with_setup("add_folder", |root| {
        fs::write(root.join("src/Value.txt"), "initial").unwrap();
    });
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let id = read
        .instances
        .values()
        .find(|instance| instance.name == "Value")
        .unwrap()
        .id;
    let file = session.path().join("src/Value.txt");
    for iteration in 0..3 {
        let disk_value = format!("filesystem {iteration}\n");
        fs::write(&file, &disk_value).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let cursor = loop {
            let read = session.get_api_read(id).unwrap();
            if read.instances[&id].properties[&ustr("Value")].as_ref()
                == &Variant::String(disk_value.clone())
            {
                break read.message_cursor;
            }
            assert!(
                Instant::now() < deadline,
                "Filesystem text edit did not reach the server"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let studio_value = format!("studio {iteration}\n");
        let response = session.post_api_write(&WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(cursor),
            authority: WriteAuthority::Filesystem,
            added: Default::default(),
            removed: Vec::new(),
            updated: vec![InstanceUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_metadata: None,
                changed_properties: UstrMap::from_iter([(
                    ustr("Value"),
                    Some(Variant::String(studio_value.clone())),
                )]),
            }],
        });
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(
            response.conflicts.updated_instances.is_empty(),
            "{:?}",
            response.conflicts
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), studio_value);
        let read = session.get_api_read(id).unwrap();
        assert_eq!(
            read.instances[&id].properties[&ustr("Value")].as_ref(),
            &Variant::String(studio_value)
        );
    }
}

#[test]
fn filesystem_module_rename_converges_and_keeps_followup_source_edits() {
    use crate::rojo_test::serve_util::TestServeSession;
    let mut session = TestServeSession::with_setup("add_folder", |root| {
        fs::write(
            root.join(".luaurc"),
            r#"{"aliases":{"target":"src/Target"}}"#,
        )
        .unwrap();
        fs::create_dir(root.join("src/Target")).unwrap();
        fs::write(root.join("src/Target/init.luau"), "return {}\n").unwrap();
        for index in 0..256 {
            fs::write(
                root.join(format!("src/Consumer{index}.luau")),
                "return require(\"@target\")\n",
            )
            .unwrap();
        }
        fs::write(root.join("src/Module.luau"), "return { Revision = 1 }\n").unwrap();
    });
    let info = session.wait_to_come_online();
    let old = session.path().join("src/Module.luau");
    let renamed = session.path().join("src/Module123.luau");
    fs::rename(&old, &renamed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (id, cursor) = loop {
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let module = read
            .instances
            .values()
            .find(|instance| instance.name == "Module123");
        if !read
            .instances
            .values()
            .any(|instance| instance.name == "Module")
        {
            if let Some(module) = module {
                break (module.id, read.message_cursor);
            }
        }
        assert!(Instant::now() < deadline, "Renamed module did not converge");
        thread::sleep(Duration::from_millis(20));
    };
    let source = "return { Revision = 2 }\n";
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(cursor),
        authority: WriteAuthority::Filesystem,
        added: Default::default(),
        removed: Vec::new(),
        updated: vec![InstanceUpdate {
            id,
            changed_name: None,
            changed_class_name: None,
            changed_parent: None,
            changed_metadata: None,
            changed_properties: UstrMap::from_iter([(
                ustr("Source"),
                Some(Variant::String(source.into())),
            )]),
        }],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert!(
        response.conflicts.updated_instances.is_empty(),
        "{:?}",
        response.conflicts
    );
    assert_eq!(fs::read_to_string(&renamed).unwrap(), source);
    let disk_source = "return { Revision = 3 }\n";
    fs::write(&renamed, disk_source).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = session.get_api_read(info.root_instance_id).unwrap();
        if let Some(module) = read.instances.get(&id) {
            if module.properties[&ustr("Source")].as_ref() == &Variant::String(disk_source.into()) {
                assert_eq!(module.name, "Module123");
                assert!(!old.exists());
                assert!(renamed.exists());
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Follow-up source edit was lost after rename"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn package_sources_serve_routes_originals_and_watches_without_reinstall() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    let mut session = TestServeSession::with_setup("add_folder", fixture::setup);
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let id = read
        .instances
        .values()
        .find(|instance| instance.name == "foo")
        .unwrap()
        .id;
    let before = fixture::files(session.path());
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: Vec::new(),
        added: Default::default(),
        updated: vec![
            fixture::source_update(id, "return { Value = 11 }\n"),
            fixture::source_update(
                read.instances
                    .values()
                    .find(|instance| instance.name == "Child")
                    .unwrap()
                    .id,
                "return 13\n",
            ),
        ],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let mut expected = before;
    expected.insert(
        fixture::ORIGINAL.into(),
        b"return { Value = 11 }\n".to_vec(),
    );
    expected.insert(
        "Source/ForkedPackages/foo/Source/Foo/Child.luau".into(),
        b"return 13\n".to_vec(),
    );
    assert_eq!(fixture::files(session.path()), expected);
    fs::write(
        session.path().join(fixture::ORIGINAL),
        "return { Value = 12 }\n",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = session.get_api_read(info.root_instance_id).unwrap();
        if read.instances.get(&id).is_some_and(|instance| {
            instance
                .properties
                .get(&ustr("Source"))
                .map(|value| value.as_ref())
                == Some(&Variant::String("return { Value = 12 }\n".into()))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Original package source was not watched"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let child = session
        .path()
        .join("Source/ForkedPackages/foo/Source/Foo/Added.luau");
    fs::write(&child, "return 99\n").unwrap();
    for present in [true, false] {
        if !present {
            fs::remove_file(&child).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let read = session.get_api_read(info.root_instance_id).unwrap();
            if read
                .instances
                .values()
                .any(|instance| instance.name == "Added")
                == present
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Original package add/remove was not watched"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn package_sources_serve_blocks_registry_generated_and_structural_writes_without_reversion() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    for target in ["registry", "Foo"] {
        let mut session = TestServeSession::with_setup("add_folder", fixture::setup);
        let info = session.wait_to_come_online();
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let id = read
            .instances
            .values()
            .find(|instance| instance.name == target)
            .unwrap()
            .id;
        let before = fixture::files(session.path());
        for remove in [false, true] {
            let read = session.get_api_read(info.root_instance_id).unwrap();
            let response = session.post_api_write(&WriteRequest {
                session_id: info.session_id,
                message_cursor: Some(read.message_cursor),
                authority: WriteAuthority::Studio,
                removed: if remove { vec![id] } else { vec![] },
                added: Default::default(),
                updated: if remove {
                    vec![]
                } else {
                    vec![InstanceUpdate {
                        id,
                        changed_name: Some("Renamed".into()),
                        changed_class_name: None,
                        changed_parent: None,
                        changed_metadata: None,
                        changed_properties: UstrMap::from_iter([(
                            ustr("Source"),
                            Some(Variant::String("return 'changed'\n".into())),
                        )]),
                    }]
                },
            });
            assert!(!response.errors.is_empty());
            assert!(
                response.conflicts.updated_instances.is_empty()
                    && response.conflicts.removed_instances.is_empty()
                    && response.conflicts.added_instances.is_empty(),
                "A warning must not revert Studio {response:?}"
            );
            assert_eq!(fixture::files(session.path()), before);
        }
    }
}

#[test]
fn package_sources_serve_preserves_stale_source_edits_without_restoring_studio() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    let mut session = TestServeSession::with_setup("add_folder", fixture::setup);
    let info = session.wait_to_come_online();
    let original = session.get_api_read(info.root_instance_id).unwrap();
    let id = original
        .instances
        .values()
        .find(|instance| instance.name == "foo")
        .unwrap()
        .id;
    fs::write(session.path().join(fixture::ORIGINAL), "return 22\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = session.get_api_read(info.root_instance_id).unwrap();
        if read.message_cursor != original.message_cursor {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    let before = fixture::files(session.path());
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(original.message_cursor),
        authority: WriteAuthority::Studio,
        removed: vec![],
        added: Default::default(),
        updated: vec![fixture::source_update(id, "return 33\n")],
    });
    assert!(!response.errors.is_empty());
    assert!(
        response.conflicts.updated_instances.is_empty()
            && response.conflicts.removed_instances.is_empty()
            && response.conflicts.added_instances.is_empty()
    );
    assert_eq!(fixture::files(session.path()), before);
}

#[test]
fn package_sources_serve_reverses_generated_imports_and_rejects_ambiguous_edits() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    let original = "local Child = require('@children/Child')\nreturn Child + 1\n";
    let mut session = TestServeSession::with_setup("add_folder", |root| {
        fixture::setup(root);
        fs::write(
            root.join(".luaurc"),
            r#"{"aliases":{"packages":"Packages","shared":"Source/Shared","children":"Source/ForkedPackages/foo/Source/Foo"}}"#,
        )
        .unwrap();
        fs::write(root.join(fixture::ORIGINAL), original).unwrap();
    });
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let instance = read
        .instances
        .values()
        .find(|instance| instance.name == "foo")
        .unwrap();
    let id = instance.id;
    let emitted = match instance.properties[&ustr("Source")].as_ref() {
        Variant::String(source) => source.clone(),
        _ => panic!("Missing source"),
    };
    assert!(
        emitted.contains("@game/ReplicatedStorage/Packages/_Index/scope_foo@local/foo/Child"),
        "{emitted}"
    );
    let mut expected = fixture::files(session.path());
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: vec![],
        added: Default::default(),
        updated: vec![fixture::source_update(
            id,
            &emitted.replace("Child + 1", "Child + 2"),
        )],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    expected.insert(
        fixture::ORIGINAL.into(),
        original.replace("Child + 1", "Child + 2").into_bytes(),
    );
    assert_eq!(fixture::files(session.path()), expected);
    let deadline = Instant::now() + Duration::from_secs(10);
    let read = loop {
        let read = session.get_api_read(info.root_instance_id).unwrap();
        if read.instances[&id].properties[&ustr("Source")].as_ref()
            == &Variant::String(emitted.replace("Child + 1", "Child + 2"))
        {
            break read;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    };
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Studio,
        removed: vec![],
        added: Default::default(),
        updated: vec![fixture::source_update(
            id,
            &emitted.replace("/foo/Child", "/foo/Other"),
        )],
    });
    assert!(!response.errors.is_empty());
    assert!(response.conflicts.updated_instances.is_empty());
    assert_eq!(fixture::files(session.path()), expected);
}

#[test]
fn package_sources_serve_invalid_or_absent_provenance_fails_closed() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    for case in fixture::INVALID_INSTALL_CASES {
        let mut session = TestServeSession::with_setup("add_folder", |root| {
            fixture::setup(root);
            fixture::invalidate(root, case);
        });
        let info = session.wait_to_come_online();
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let id = read
            .instances
            .values()
            .find(|instance| instance.name == "registry")
            .unwrap()
            .id;
        let before = fixture::files(session.path());
        let response = session.post_api_write(&WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(read.message_cursor),
            authority: WriteAuthority::Studio,
            removed: vec![],
            added: Default::default(),
            updated: vec![fixture::source_update(id, "return 'changed'\n")],
        });
        assert!(!response.errors.is_empty(), "{case}");
        assert!(
            response.conflicts.updated_instances.is_empty()
                && response.conflicts.added_instances.is_empty()
                && response.conflicts.removed_instances.is_empty()
        );
        assert_eq!(fixture::files(session.path()), before, "{case}");
    }
}

#[test]
fn package_sources_serve_add_rename_and_remove_use_original_directories() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    use librojo::web_api::InstanceAddition;
    let mut session = TestServeSession::with_setup("add_folder", fixture::setup);
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let parent = read
        .instances
        .values()
        .find(|instance| instance.name == "foo")
        .unwrap()
        .id;
    let before = fixture::files(session.path());
    let (model, ids) = created_subtree("Added", &[("Module", "ModuleScript")]);
    let folder_id = ids.id;
    let module_id = ids.children[0].id;
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: vec![],
        updated: vec![],
        added: std::collections::HashMap::from([(
            folder_id,
            InstanceAddition { parent, model, ids },
        )]),
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let original = session
        .path()
        .join("Source/ForkedPackages/foo/Source/Foo/Added");
    assert_eq!(
        fs::read_to_string(original.join("Module.luau")).unwrap(),
        "return \"Module\"\n"
    );
    assert!(!session
        .path()
        .join(fixture::INSTALLED)
        .join("Added")
        .exists());
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let mut rename = fixture::source_update(module_id, "return \"Module\"\n");
    rename.changed_name = Some("Renamed".into());
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: vec![],
        added: Default::default(),
        updated: vec![rename],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert!(original.join("Renamed.luau").is_file());
    assert!(!original.join("Module.luau").exists());
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: vec![folder_id],
        added: Default::default(),
        updated: vec![],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert!(!original.exists());
    assert_eq!(fixture::files(session.path()), before);
}

#[test]
fn package_sources_serve_batches_model_properties_with_script_sources() {
    use crate::rojo_test::{package_sources as fixture, serve_util::TestServeSession};
    let model_path = "Source/ForkedPackages/foo/Source/Foo/Thing.model.json";
    let mut session = TestServeSession::with_setup("add_folder", |root| {
        fixture::setup(root);
        fs::write(root.join(model_path), r#"{"className":"Folder","children":[{"name":"Nested","className":"NumberValue","properties":{"Value":3}}]}"#).unwrap();
    });
    let info = session.wait_to_come_online();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let script = read
        .instances
        .values()
        .find(|instance| instance.name == "foo")
        .unwrap()
        .id;
    let child = read
        .instances
        .values()
        .find(|instance| instance.name == "Nested")
        .unwrap()
        .id;
    let mut model = fixture::source_update(child, "");
    model.changed_properties = UstrMap::from_iter([(ustr("Value"), Some(Variant::Float64(4.0)))]);
    let mut expected = fixture::files(session.path());
    let response = session.post_api_write(&WriteRequest {
        session_id: info.session_id,
        message_cursor: Some(read.message_cursor),
        authority: WriteAuthority::Filesystem,
        removed: vec![],
        added: Default::default(),
        updated: vec![fixture::source_update(script, "return 23\n"), model],
    });
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let model = fs::read(session.path().join(model_path)).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&model).unwrap();
    assert_eq!(value["children"][0]["properties"]["Value"], 4);
    expected.insert(model_path.into(), model);
    expected.insert(fixture::ORIGINAL.into(), b"return 23\n".to_vec());
    assert_eq!(fixture::files(session.path()), expected);
}

#[test]
fn numeric_api_writes_normalize_disk_json_and_converge() {
    assert_numeric_api_writes_converge(&[(5.0000005, 5.0), (-5.0000005, -5.0)]);
}

#[test]
fn numeric_api_writes_normalize_near_zero_and_converge() {
    assert_numeric_api_writes_converge(&[(0.0000005, 0.0), (-0.0000005, 0.0)]);
}

#[test]
fn numeric_api_writes_emit_whole_values_as_integers() {
    assert_numeric_api_writes_converge(&[(8.0, 8.0)]);
}

#[test]
fn numeric_api_writes_preserve_fractional_values_and_converge() {
    assert_numeric_api_writes_converge(&[(8.000002, 8.000002), (-0.000002, -0.000002)]);
}

fn assert_numeric_api_writes_converge(cases: &[(f64, f64)]) {
    run_serve_test("add_folder", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;
        let numeric_path = session.path().join("src/Numeric");
        fs::create_dir(&numeric_path).unwrap();
        let metadata_path = numeric_path.join("init.meta.json");
        fs::write(
            &metadata_path,
            r#"{"className":"NumberValue","properties":{"Value":0.25}}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let id =
            loop {
                let read = session.get_api_read(root_id).unwrap();
                if let Some(instance) = read.instances.values().find(|instance| {
                    instance.name == "Numeric" && instance.class_name == "NumberValue"
                }) {
                    break instance.id;
                }
                assert!(
                    Instant::now() < deadline,
                    "Numeric metadata was not loaded."
                );
                thread::sleep(Duration::from_millis(20));
            };

        for &(input, expected) in cases {
            let mut saved = None;
            for attempt in 0..3 {
                let read = session.get_api_read(root_id).unwrap();
                let response = session.post_api_write(&WriteRequest {
                    session_id: info.session_id,
                    message_cursor: Some(read.message_cursor),
                    authority: WriteAuthority::Filesystem,
                    removed: Vec::new(),
                    added: Default::default(),
                    updated: vec![InstanceUpdate {
                        id,
                        changed_name: None,
                        changed_class_name: None,
                        changed_parent: None,
                        changed_metadata: None,
                        changed_properties: UstrMap::from_iter([(
                            ustr("Value"),
                            Some(Variant::Float64(input)),
                        )]),
                    }],
                });
                assert_eq!(response.session_id, info.session_id);
                assert!(
                    response.errors.is_empty(),
                    "Input {input}, attempt {attempt}, errors {:?}",
                    response.errors
                );
                assert!(
                    response.conflicts.removed_instances.is_empty()
                        && response.conflicts.added_instances.is_empty()
                        && response.conflicts.updated_instances.is_empty(),
                    "Input {input}, attempt {attempt}, conflicts {:?}",
                    response.conflicts
                );
                if attempt > 0 {
                    assert!(
                        response.accepted.removed_instances.is_empty()
                            && response.accepted.added_instances.is_empty()
                            && response.accepted.updated_instances.is_empty(),
                        "Repeated input {input} must already be synchronized, got {:?}.",
                        response.accepted
                    );
                }

                if attempt > 0 && input != expected {
                    let packet = session
                        .get_api_socket_packet(SocketPacketType::Messages, read.message_cursor)
                        .unwrap();
                    let SocketPacketBody::Messages(packet) = packet.body else {
                        panic!("Expected a messages packet");
                    };
                    assert!(
                        packet
                            .messages
                            .iter()
                            .flat_map(|message| &message.updated)
                            .any(|update| {
                                update.id == id
                                    && update.changed_properties.get(&ustr("Value"))
                                        == Some(&Some(Variant::Float64(expected)))
                            }),
                        "Studio must receive the canonical value for {input}."
                    );
                }
                let contents = fs::read_to_string(&metadata_path).unwrap();
                let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
                let value = &metadata["properties"]["Value"];
                // NumberValue.Value defaults to zero, so syncback may omit it.
                let omitted_default = expected == 0.0 && value.is_null();
                assert_eq!(
                    if omitted_default {
                        Some(0.0)
                    } else {
                        value.as_f64()
                    },
                    Some(expected),
                    "Saved input {input} as {contents}"
                );
                if expected.fract() == 0.0 && !omitted_default {
                    assert!(
                        value.is_i64() || value.is_u64(),
                        "Expected a literal integer for {input}, got {contents}"
                    );
                }
                if let Some(saved) = &saved {
                    assert_eq!(
                        &contents, saved,
                        "Repeated writes must not rewrite or oscillate."
                    );
                }
                for update in &response.accepted.updated_instances {
                    assert_eq!(
                        update.changed_properties.get(&ustr("Value")),
                        Some(&if expected == 0.0 {
                            None
                        } else {
                            Some(Variant::Float64(expected))
                        }),
                        "Acknowledgment and disk disagree for {input}."
                    );
                }
                let read = session.get_api_read(root_id).unwrap();
                assert_eq!(
                    read.instances[&id]
                        .properties
                        .get(&ustr("Value"))
                        .map(|value| value.as_ref())
                        .unwrap_or(&Variant::Float64(0.0)),
                    &Variant::Float64(expected),
                    "Server and disk disagree immediately after saving {input}."
                );

                // A changed attribute in the same metadata file proves its filesystem
                // feedback was processed before checking the acknowledged numeric value.
                let marker = format!("{input}-{attempt}");
                let mut feedback = metadata;
                feedback["attributes"] = serde_json::json!({"Feedback": marker});
                fs::write(&metadata_path, serde_json::to_vec(&feedback).unwrap()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let read = session.get_api_read(root_id).unwrap();
                    let properties = &read.instances[&id].properties;
                    let observed = properties.get(&ustr("Attributes")).is_some_and(|value| {
                        matches!(value.as_ref(), Variant::Attributes(attributes)
                            if attributes.get("Feedback") == Some(&Variant::String(marker.clone())))
                    });
                    if observed {
                        assert_eq!(
                            properties
                                .get(&ustr("Value"))
                                .map(|value| value.as_ref())
                                .unwrap_or(&Variant::Float64(0.0)),
                            &Variant::Float64(expected),
                            "Server and disk disagree after feedback for {input}."
                        );
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "Filesystem feedback was not processed for {input}."
                    );
                    thread::sleep(Duration::from_millis(20));
                }
                // Preserve the independent disk edit while comparing subsequent writes.
                saved = Some(fs::read_to_string(&metadata_path).unwrap());
            }
        }
    });
}

/// Builds what Studio sends for a created subtree: the model and the ids it
/// picked, shaped like the subtree. `children` lists (name, class) pairs under
/// a Folder named `name`, and every ModuleScript gets source.
fn created_subtree(name: &str, children: &[(&str, &str)]) -> (Vec<u8>, librojo::web_api::AddedIds) {
    use librojo::web_api::AddedIds;
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
    let root = dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Folder").with_name(name),
    );
    let mut ids = AddedIds {
        id: Ref::new(),
        name: name.into(),
        class_name: ustr("Folder"),
        children: Vec::new(),
    };
    for (child, class) in children {
        let mut builder = InstanceBuilder::new(*class).with_name(*child);
        if *class == "ModuleScript" {
            builder =
                builder.with_property("Source", Variant::String(format!("return \"{child}\"\n")));
        }
        dom.insert(root, builder);
        ids.children.push(AddedIds {
            id: Ref::new(),
            name: (*child).into(),
            class_name: ustr(class),
            children: Vec::new(),
        });
    }
    let mut model = Vec::new();
    rbx_binary::to_writer(&mut model, &dom, &[root]).unwrap();
    (model, ids)
}

fn post_write(
    session: &crate::rojo_test::serve_util::TestServeSession,
    request: &WriteRequest,
) -> librojo::web_api::WriteResponse {
    let response = reqwest::blocking::Client::new()
        .post(format!("http://localhost:{}/api/write", session.port()))
        .header("content-type", "application/msgpack")
        .body(serialize_msgpack(request).unwrap())
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .unwrap();
    deserialize_msgpack(&response).unwrap()
}

// A subtree created in Studio is written as new files and keeps the ids
// Studio picked, so the server's announcement of it isn't a second copy, and
// the watcher seeing the new files changes nothing.
#[test]
fn studio_created_subtrees_are_written_as_directories_and_keep_their_ids() {
    assert_created_subtrees_are_written("place_root", |directory| {
        assert!(
            directory.is_dir(),
            "{} was not created",
            directory.display()
        );
        assert_eq!(
            fs::read_to_string(directory.join("Code.luau")).unwrap(),
            "return \"Code\"\n"
        );
        assert!(directory.join("Same").is_dir());
        assert!(directory.join("Same~2").is_dir());
    });
}

// Without instancesAsDirectories, a folder with same-named children can't be
// a directory, so it's written as a model file, like a full syncback would.
#[test]
fn studio_created_subtrees_fall_back_to_models_and_keep_their_ids() {
    assert_created_subtrees_are_written("place_root_models", |directory| {
        assert!(!directory.exists());
        assert!(directory.with_extension("rbxm").is_file());
    });
}

fn assert_created_subtrees_are_written(fixture: &str, check_layout: impl Fn(&std::path::Path)) {
    use librojo::web_api::InstanceAddition;
    use std::collections::{HashMap, HashSet};

    run_serve_test(fixture, |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let read = session.get_api_read(root).unwrap();
        let storage = read
            .instances
            .values()
            .find(|instance| instance.name == "ReplicatedStorage")
            .unwrap()
            .id;
        let cursor = read.message_cursor;

        let mut added = HashMap::new();
        let mut expected_ids = HashSet::new();
        for (parent, name) in [(storage, "Created"), (root, "AtRoot")] {
            let (model, ids) = created_subtree(
                name,
                &[
                    ("Code", "ModuleScript"),
                    ("Same", "Folder"),
                    ("Same", "Folder"),
                ],
            );
            expected_ids.insert(ids.id);
            expected_ids.extend(ids.children.iter().map(|child| child.id));
            added.insert(ids.id, InstanceAddition { parent, model, ids });
        }
        let response = post_write(
            &session,
            &WriteRequest {
                session_id: info.session_id,
                message_cursor: Some(cursor),
                authority: WriteAuthority::Filesystem,
                removed: Vec::new(),
                added,
                updated: Vec::new(),
            },
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);

        let src = session.path().join("src");
        for directory in [src.join("ReplicatedStorage/Created"), src.join("AtRoot")] {
            check_layout(&directory);
        }

        // Let the watcher report the new files, then make sure every instance
        // is still the one Studio named, exactly once.
        thread::sleep(Duration::from_millis(1500));
        let read = session.get_api_read(root).unwrap();
        for id in &expected_ids {
            assert!(read.instances.contains_key(id), "lost the id {id:?}");
        }
        let created = read
            .instances
            .values()
            .filter(|instance| instance.name == "Created" || instance.name == "AtRoot")
            .count();
        assert_eq!(created, 2);
        let packet = session
            .get_api_socket_packet(SocketPacketType::Messages, cursor)
            .unwrap();
        let SocketPacketBody::Messages(packet) = packet.body else {
            panic!("Expected a messages packet");
        };
        for message in &packet.messages {
            for id in message.added.keys() {
                assert!(
                    expected_ids.contains(id),
                    "announced an unexpected instance {id:?}"
                );
            }
        }
    });
}

/// Waits up to a few seconds for `check` to hold.
fn eventually(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    check()
}

/// An empty write, which only tells the server which side wins.
fn announce_authority(
    session: &crate::rojo_test::serve_util::TestServeSession,
    authority: WriteAuthority,
) {
    let info = session.get_api_rojo().unwrap();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    let response = post_write(
        session,
        &WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(read.message_cursor),
            authority,
            removed: Vec::new(),
            added: Default::default(),
            updated: Vec::new(),
        },
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
}

fn set_model_trees(session: &crate::rojo_test::serve_util::TestServeSession, rules: &str) {
    set_syncback_rules(session, &format!(r#""modelTrees":{rules}"#));
}

/// Rewrites the `place_root` project with `instancesAsDirectories` and the
/// given extra syncback rules.
fn set_syncback_rules(session: &crate::rojo_test::serve_util::TestServeSession, rules: &str) {
    use std::io::Write;

    // Publish one complete project revision, as an editor's atomic save does,
    // rather than exposing a truncated project to the native watcher.
    let mut staged = tempfile::NamedTempFile::new_in(session.path()).unwrap();
    write!(
        staged,
        r#"{{"name":"place_root","tree":{{"$className":"DataModel","$path":"src"}},"syncbackRules":{{"instancesAsDirectories":true,{rules}}}}}"#
    )
    .unwrap();
    staged
        .persist(session.path().join("default.project.json"))
        .unwrap();
}

/// Waits for an instance named `name` and returns its ID.
fn wait_for_instance(session: &crate::rojo_test::serve_util::TestServeSession, name: &str) -> Ref {
    let root = session.get_api_rojo().unwrap().root_instance_id;
    let mut found = Ref::none();
    assert!(
        eventually(|| {
            let read = session.get_api_read(root).unwrap();
            match read.instances.values().find(|i| i.name == name) {
                Some(instance) => {
                    found = instance.id;
                    true
                }
                None => false,
            }
        }),
        "{name} never appeared"
    );
    found
}

fn parent_of(session: &crate::rojo_test::serve_util::TestServeSession, id: Ref) -> Option<Ref> {
    let root = session.get_api_rojo().unwrap().root_instance_id;
    let read = session.get_api_read(root).unwrap();
    read.instances.get(&id).map(|instance| instance.parent)
}

// A rule can match an instance stored in a folder's metadata. Its outermost
// ancestor in that file is written out as files, and every ID is kept.
#[test]
fn model_tree_rules_reach_instances_stored_in_metadata() {
    use crate::rojo_test::serve_util::TestServeSession;

    // Only the rule change belongs to the watcher exercise. Establish the
    // metadata fixture before startup, not through another live watcher event.
    let mut session = TestServeSession::with_setup("place_root", |root| {
        fs::write(
            root.join("src/ReplicatedStorage/Mover/init.meta.json"),
            r#"{"children":[{"name":"Outer","className":"Folder","children":[{"name":"FX","className":"Folder","children":[{"name":"Spark","className":"Part"}]}]}]}"#,
        )
        .unwrap();
    });
    session.wait_to_come_online();
    let mover_dir = session.path().join("src/ReplicatedStorage/Mover");
    let mover = wait_for_instance(&session, "Mover");
    let spark = wait_for_instance(&session, "Spark");
    let fx = wait_for_instance(&session, "FX");
    let outer = wait_for_instance(&session, "Outer");
    announce_authority(&session, WriteAuthority::Studio);

    set_model_trees(
        &session,
        r#"[{"pattern":"ReplicatedStorage/Mover/Outer/FX"}]"#,
    );
    assert!(
        eventually(|| mover_dir.join("Outer/FX.rbxm").is_file()),
        "FX never became a model"
    );
    // The model is written before the metadata that held it.
    assert!(eventually(|| !fs::read_to_string(
        mover_dir.join("init.meta.json")
    )
    .unwrap()
    .contains("Outer")));
    let model =
        rbx_binary::from_reader(fs::File::open(mover_dir.join("Outer/FX.rbxm")).unwrap()).unwrap();
    let root = model.root();
    assert_eq!(root.children().len(), 1);
    let model_fx = model.get_by_ref(root.children()[0]).unwrap();
    assert_eq!(model_fx.name, "FX");
    assert_eq!(model_fx.class, "Folder");
    assert_eq!(model_fx.children().len(), 1);
    let model_spark = model.get_by_ref(model_fx.children()[0]).unwrap();
    assert_eq!(model_spark.name, "Spark");
    assert_eq!(model_spark.class, "Part");
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(parent_of(&session, outer), Some(mover));
    assert_eq!(parent_of(&session, fx), Some(outer));
    assert_eq!(parent_of(&session, spark), Some(fx));
}

// Changing the modelTrees rules while serving moves matching instances to
// their new layout when Studio is authoritative, and back when the rule goes,
// keeping their IDs. Under Filesystem authority, layouts never change. Before
// any write says which side wins, the first one decides.
#[test]
fn model_tree_rule_changes_relayout_only_under_studio_authority() {
    use crate::rojo_test::serve_util::TestServeSession;

    for case in ["studio", "filesystem", "studio_later"] {
        let mut session = TestServeSession::with_setup("place_root", |root| {
            fs::write(
                root.join("src/ReplicatedStorage/Mover/Kid.luau"),
                "return 'kid'",
            )
            .unwrap();
        });
        let info = session.wait_to_come_online();
        let storage = session.path().join("src/ReplicatedStorage");
        let mover = session
            .get_api_read(info.root_instance_id)
            .unwrap()
            .instances
            .values()
            .find(|instance| instance.name == "Mover")
            .unwrap()
            .id;
        let kid = wait_for_instance(&session, "Kid");
        let both_kept = || {
            let read = session.get_api_read(info.root_instance_id).unwrap();
            read.instances.contains_key(&mover)
                && read.instances.get(&kid).is_some_and(|i| i.parent == mover)
                && read
                    .instances
                    .values()
                    .filter(|i| i.name == "Mover")
                    .count()
                    == 1
                && read.instances.values().filter(|i| i.name == "Kid").count() == 1
        };
        match case {
            "studio" => announce_authority(&session, WriteAuthority::Studio),
            "filesystem" => announce_authority(&session, WriteAuthority::Filesystem),
            _ => {}
        }

        set_model_trees(&session, r#"[{"pattern":"ReplicatedStorage/Mover"}]"#);
        if case == "studio_later" {
            thread::sleep(Duration::from_millis(1000));
            assert!(storage.join("Mover").is_dir(), "moved before any write");
            announce_authority(&session, WriteAuthority::Studio);
        }

        if case == "filesystem" {
            thread::sleep(Duration::from_millis(1500));
            assert!(storage.join("Mover").is_dir(), "{case}");
            assert!(!storage.join("Mover.rbxm").exists(), "{case}");
            continue;
        }
        assert!(
            eventually(|| storage.join("Mover.rbxm").is_file()),
            "{case} never wrote the model"
        );
        assert!(!storage.join("Mover").exists(), "{case}");
        thread::sleep(Duration::from_millis(1000));
        assert!(both_kept(), "{case} lost an ID in the model");

        // Without a rule, instancesAsDirectories turns it back.
        set_model_trees(&session, "[]");
        assert!(
            eventually(|| storage.join("Mover").is_dir()),
            "{case} never went back"
        );
        assert!(!storage.join("Mover.rbxm").exists(), "{case}");
        assert_eq!(
            fs::read_to_string(storage.join("Mover/Kid.luau")).unwrap(),
            "return 'kid'"
        );
        thread::sleep(Duration::from_millis(1000));
        assert!(both_kept(), "{case} lost an ID in the directories");
    }
}

// A folder syncback kept with a `.gitkeep` doesn't need it once something is
// created in it from Studio.
#[test]
fn studio_created_subtrees_remove_the_gitkeep_they_replace() {
    use librojo::web_api::InstanceAddition;
    use std::collections::HashMap;

    run_serve_test("place_root", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let mover = read
            .instances
            .values()
            .find(|instance| instance.name == "Mover")
            .unwrap()
            .id;
        let keep = session.path().join("src/ReplicatedStorage/Mover/.gitkeep");
        assert!(keep.is_file());

        let (model, ids) = created_subtree("Created", &[]);
        let response = post_write(
            &session,
            &WriteRequest {
                session_id: info.session_id,
                message_cursor: Some(read.message_cursor),
                authority: WriteAuthority::Filesystem,
                removed: Vec::new(),
                added: HashMap::from([(
                    ids.id,
                    InstanceAddition {
                        parent: mover,
                        model,
                        ids,
                    },
                )]),
                updated: Vec::new(),
            },
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(!keep.exists());
        assert!(session
            .path()
            .join("src/ReplicatedStorage/Mover/Created")
            .is_dir());
    });
}

#[test]
fn studio_created_subtrees_reject_ids_that_are_not_new() {
    use librojo::web_api::InstanceAddition;
    use std::collections::HashMap;

    run_serve_test("place_root", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let read = session.get_api_read(root).unwrap();
        let (model, mut ids) = created_subtree("Reused", &[("Child", "Folder")]);
        // Studio reusing an id the server already has must not be accepted.
        ids.children[0].id = root;
        let response = post_write(
            &session,
            &WriteRequest {
                session_id: info.session_id,
                message_cursor: Some(read.message_cursor),
                authority: WriteAuthority::Filesystem,
                removed: Vec::new(),
                added: HashMap::from([(
                    ids.id,
                    InstanceAddition {
                        parent: root,
                        model,
                        ids: ids.clone(),
                    },
                )]),
                updated: Vec::new(),
            },
        );
        assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
        assert_eq!(response.errors[0].id, ids.id);
        assert!(!session.path().join("src/Reused").exists());
    });
}

// The DataModel of a place project records the project file as its source, so
// moving a folder to the root used to be refused as a script promotion.
#[test]
fn studio_move_to_the_datamodel_root_writes_into_its_path() {
    use librojo::web_api::WriteResponse;
    use std::collections::HashMap;

    run_serve_test("place_root", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let read = session.get_api_read(root).unwrap();
        let mover = read
            .instances
            .values()
            .find(|instance| instance.name == "Mover")
            .expect("The fixture must contain Mover")
            .id;

        let request = WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(read.message_cursor),
            authority: WriteAuthority::Studio,
            removed: Vec::new(),
            added: HashMap::new(),
            updated: vec![InstanceUpdate {
                id: mover,
                changed_parent: Some(root),
                changed_name: None,
                changed_class_name: None,
                changed_metadata: None,
                changed_properties: UstrMap::default(),
            }],
        };
        let response = reqwest::blocking::Client::new()
            .post(format!("http://localhost:{}/api/write", session.port()))
            .header("content-type", "application/msgpack")
            .body(serialize_msgpack(&request).unwrap())
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .unwrap();
        let response: WriteResponse = deserialize_msgpack(&response).unwrap();
        assert!(response.errors.is_empty(), "{:?}", response.errors);

        assert!(!session.path().join("src/ReplicatedStorage/Mover").exists());
        assert!(session.path().join("src/Mover").is_dir());
        let moved = session.get_api_read(root).unwrap();
        assert_eq!(moved.instances[&mover].parent, root);
    });
}

/// The `shared_files` fixture, read from a running server, with what the
/// tests below look up in it.
struct SharedFiles {
    info: librojo::web_api::ServerInfoResponse,
    cursor: u32,
    ids: std::collections::HashMap<String, Ref>,
}

impl SharedFiles {
    fn read(session: &crate::rojo_test::serve_util::TestServeSession) -> Self {
        let info = session.get_api_rojo().unwrap();
        let read = session.get_api_read(info.root_instance_id).unwrap();
        let ids = read
            .instances
            .values()
            .map(|instance| (instance.name.to_string(), instance.id))
            .collect();
        SharedFiles {
            cursor: read.message_cursor,
            info,
            ids,
        }
    }

    fn id(&self, name: &str) -> Ref {
        self.ids[name]
    }

    fn write(
        &self,
        session: &crate::rojo_test::serve_util::TestServeSession,
        removed: Vec<Ref>,
        updated: Vec<InstanceUpdate>,
    ) -> librojo::web_api::WriteResponse {
        post_write(
            session,
            &WriteRequest {
                session_id: self.info.session_id,
                message_cursor: Some(self.cursor),
                authority: WriteAuthority::Filesystem,
                removed,
                added: Default::default(),
                updated,
            },
        )
    }
}

fn shared_update(id: Ref) -> InstanceUpdate {
    InstanceUpdate {
        id,
        changed_parent: None,
        changed_name: None,
        changed_class_name: None,
        changed_metadata: None,
        changed_properties: UstrMap::default(),
    }
}

/// Waits for the watcher to report a write, then returns every message the
/// server sent after `cursor`.
fn settled_messages(
    session: &crate::rojo_test::serve_util::TestServeSession,
    cursor: u32,
) -> Vec<librojo::web_api::SubscribeMessage<'static>> {
    thread::sleep(Duration::from_millis(1500));
    // The socket waits for a message, so it's only asked when there is one.
    let root = session.get_api_rojo().unwrap().root_instance_id;
    if session.get_api_read(root).unwrap().message_cursor == cursor {
        return Vec::new();
    }
    let packet = session
        .get_api_socket_packet(SocketPacketType::Messages, cursor)
        .unwrap();
    let SocketPacketBody::Messages(packet) = packet.body else {
        panic!("Expected a messages packet");
    };
    packet.messages
}

fn read_ref(
    session: &crate::rojo_test::serve_util::TestServeSession,
    id: Ref,
    property: &str,
) -> Option<Ref> {
    let info = session.get_api_rojo().unwrap();
    let read = session.get_api_read(info.root_instance_id).unwrap();
    match read.instances[&id]
        .properties
        .get(&ustr(property))
        .map(|value| value.as_ref())
    {
        Some(Variant::Ref(referent)) if referent.is_some() => Some(*referent),
        _ => None,
    }
}

// An edit inside a binary model rewrites the file, but every instance in it is
// still the one Studio has. Replacing them would make Studio rebuild the model,
// and a re-read of the file must not report its references as changes.
#[test]
fn studio_edits_inside_binary_models_keep_every_instance() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_name: Some("Renamed".into()),
                ..shared_update(fixture.id("A"))
            }],
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(response.conflicts.updated_instances.is_empty());

        // Other connected Studios hear about the rename, and nothing else.
        let mut renamed = false;
        for message in settled_messages(&session, fixture.cursor) {
            assert!(message.removed.is_empty(), "{:?}", message.removed);
            assert!(message.added.is_empty(), "{:?}", message.added.keys());
            for update in &message.updated {
                assert!(
                    update.changed_properties.is_empty(),
                    "re-reading the model reported {:?}",
                    update.changed_properties.keys()
                );
                renamed |= update.id == fixture.id("A")
                    && update.changed_name.as_deref() == Some("Renamed");
            }
        }
        assert!(renamed, "the rename was never announced");
        let weld = fixture.id("Weld");
        assert_eq!(read_ref(&session, weld, "Part0"), Some(fixture.id("A")));
        assert_eq!(read_ref(&session, weld, "Part1"), Some(fixture.id("B")));

        // The attribute the model's metadata set moves into the model file,
        // which now holds the root's properties, and it's still there.
        let storage = session.path().join("src/ReplicatedStorage");
        assert!(!storage.join("BinModel.meta.json").exists());
        let read = session.get_api_read(fixture.info.root_instance_id).unwrap();
        let attributes = read.instances[&fixture.id("BinModel")]
            .properties
            .get(&ustr("Attributes"))
            .map(|value| value.as_ref().clone());
        let Some(Variant::Attributes(attributes)) = attributes else {
            panic!("The model lost its attributes");
        };
        // Binary models read string attributes back as binary strings.
        let kind = match attributes.get("Kind") {
            Some(Variant::String(kind)) => kind.as_bytes().to_vec(),
            Some(Variant::BinaryString(kind)) => AsRef::<[u8]>::as_ref(kind).to_vec(),
            other => panic!("unexpected Kind {other:?}"),
        };
        assert_eq!(kind, b"binary");
    });
}

// A move between two model files rewrites both, and the moved instances keep
// their ids.
#[test]
fn studio_moves_between_model_files_keep_their_ids() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let inner = fixture.id("Inner");
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_parent: Some(fixture.id("BinModel")),
                ..shared_update(inner)
            }],
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);

        let json = fs::read_to_string(
            session
                .path()
                .join("src/ReplicatedStorage/JsonModel.model.json"),
        )
        .unwrap();
        assert!(!json.contains("Inner"), "{json}");
        settled_messages(&session, fixture.cursor);
        let read = session.get_api_read(fixture.info.root_instance_id).unwrap();
        assert_eq!(read.instances[&inner].parent, fixture.id("BinModel"));
        assert_eq!(read.instances[&fixture.id("Value")].parent, inner);
    });
}

// A reference whose target moves to another file follows it through the
// target's `Rojo_Id`, which the move keeps.
#[test]
fn studio_references_follow_targets_with_ids_into_other_files() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let pointer = fixture.id("Pointer");
        let base = fixture.id("Base");
        assert_eq!(read_ref(&session, pointer, "Value"), Some(base));
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_parent: Some(fixture.id("ReplicatedStorage")),
                ..shared_update(base)
            }],
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        let storage = session.path().join("src/ReplicatedStorage");
        assert!(storage.join("Base").is_dir());
        let json = fs::read_to_string(storage.join("JsonModel.model.json")).unwrap();
        assert!(json.contains("Rojo_Target_Value"), "{json}");
        settled_messages(&session, fixture.cursor);
        assert_eq!(read_ref(&session, pointer, "Value"), Some(base));
    });
}

// A binary model holds references natively, so its targets have no
// `Rojo_Id`. Moving one out of the file is saved, and the reference that was
// lost is reported.
#[test]
fn studio_references_to_targets_without_ids_are_reported() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_parent: Some(fixture.id("ReplicatedStorage")),
                ..shared_update(fixture.id("A"))
            }],
        );
        assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
        assert_eq!(response.errors[0].id, fixture.id("Weld"));
        assert_eq!(response.errors[0].property.as_deref(), Some("Part0"));
        assert!(session.path().join("src/ReplicatedStorage/A").is_dir());
    });
}

// Clearing a reference in a JSON model removes its link attribute, so reading
// the file back doesn't bring the reference back.
#[test]
fn studio_cleared_references_stay_cleared() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let pointer = fixture.id("Pointer");
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_properties: UstrMap::from_iter([(
                    ustr("Value"),
                    Some(Variant::Ref(Ref::none())),
                )]),
                ..shared_update(pointer)
            }],
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        let json = fs::read_to_string(
            session
                .path()
                .join("src/ReplicatedStorage/JsonModel.model.json"),
        )
        .unwrap();
        assert!(!json.contains("Rojo_Target_Value"), "{json}");
        settled_messages(&session, fixture.cursor);
        assert_eq!(read_ref(&session, pointer, "Value"), None);
    });
}

// Deleting what a reference points at leaves it nil, like in Studio, and
// isn't an error.
#[test]
fn studio_deletions_inside_model_files_clear_references() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let a = fixture.id("A");
        let response = fixture.write(&session, vec![a], Vec::new());
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        // Studio forgets a saved deletion only when it's accepted, and every
        // other connected Studio is told about it.
        assert_eq!(response.accepted.removed_instances, vec![a]);
        let messages = settled_messages(&session, fixture.cursor);
        assert!(messages.iter().any(|message| message.removed.contains(&a)));
        let weld = fixture.id("Weld");
        assert_eq!(read_ref(&session, weld, "Part0"), None);
        assert_eq!(read_ref(&session, weld, "Part1"), Some(fixture.id("B")));
    });
}

// A created subtree written as JSON links its references through attributes,
// so they survive the round trip through its files.
#[test]
fn studio_created_subtrees_keep_their_references() {
    use librojo::web_api::{AddedIds, InstanceAddition};
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use std::collections::HashMap;

    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let root = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder").with_name("Linked"),
        );
        let target = dom.insert(root, InstanceBuilder::new("Part").with_name("Target"));
        dom.insert(
            root,
            InstanceBuilder::new("ObjectValue")
                .with_name("Link")
                .with_property("Value", Variant::Ref(target)),
        );
        let mut model = Vec::new();
        rbx_binary::to_writer(&mut model, &dom, &[root]).unwrap();
        let child = |name: &str, class: &str| AddedIds {
            id: Ref::new(),
            name: name.into(),
            class_name: ustr(class),
            children: Vec::new(),
        };
        let ids = AddedIds {
            id: Ref::new(),
            name: "Linked".into(),
            class_name: ustr("Folder"),
            children: vec![child("Target", "Part"), child("Link", "ObjectValue")],
        };
        let (target_id, link_id) = (ids.children[0].id, ids.children[1].id);
        let response = post_write(
            &session,
            &WriteRequest {
                session_id: fixture.info.session_id,
                message_cursor: Some(fixture.cursor),
                authority: WriteAuthority::Filesystem,
                removed: Vec::new(),
                added: HashMap::from([(
                    ids.id,
                    InstanceAddition {
                        parent: fixture.id("ReplicatedStorage"),
                        model,
                        ids,
                    },
                )]),
                updated: Vec::new(),
            },
        );
        assert!(response.errors.is_empty(), "{:?}", response.errors);

        let linked = session.path().join("src/ReplicatedStorage/Linked");
        let mut written = String::new();
        let mut pending = vec![linked];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            } else {
                written += &fs::read_to_string(&path).unwrap();
            }
        }
        assert!(written.contains("Rojo_Target_Value"), "{written}");
        assert!(written.contains("Rojo_Id"), "{written}");
        assert_eq!(read_ref(&session, link_id, "Value"), Some(target_id));
        settled_messages(&session, fixture.cursor);
        assert_eq!(read_ref(&session, link_id, "Value"), Some(target_id));
    });
}

#[test]
fn studio_move_and_rename_preserve_ids_through_real_watcher_events() {
    use librojo::web_api::{
        InstanceUpdate, SocketPacketBody, WriteAuthority, WriteRequest, WriteResponse,
    };
    use rbx_dom_weak::{types::Variant, UstrMap};
    use std::{
        collections::HashMap,
        thread,
        time::{Duration, Instant},
    };

    run_serve_test("scripts", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let initial = session.get_api_read(root).unwrap();
        let source = initial
            .instances
            .values()
            .find(|instance| instance.name == "foo")
            .expect("The fixture must contain foo")
            .id;
        fs::create_dir(session.path().join("src/Destination")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let destination = loop {
            let response = session.get_api_read(root).unwrap();
            if let Some(instance) = response
                .instances
                .values()
                .find(|instance| instance.name == "Destination")
            {
                break instance.id;
            }
            assert!(
                Instant::now() < deadline,
                "The watcher did not observe Destination"
            );
            thread::sleep(Duration::from_millis(20));
        };
        // The read can see the new tree before its notification is published. Drain
        // fixture creation before asserting that the move itself is update-only.
        let setup = session
            .get_api_socket_packet(SocketPacketType::Messages, initial.message_cursor)
            .unwrap();
        let SocketPacketBody::Messages(setup) = setup.body else {
            panic!("Expected a messages packet");
        };
        assert!(setup
            .messages
            .iter()
            .any(|message| message.added.contains_key(&destination)));
        let cursor = setup.message_cursor;
        let request = WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(cursor),
            authority: WriteAuthority::Studio,
            removed: Vec::new(),
            added: HashMap::new(),
            updated: vec![InstanceUpdate {
                id: source,
                changed_parent: Some(destination),
                changed_name: Some("Moved".into()),
                changed_class_name: None,
                changed_metadata: None,
                changed_properties: UstrMap::default(),
            }],
        };
        let response = reqwest::blocking::Client::new()
            .post(format!("http://localhost:{}/api/write", session.port()))
            .header("content-type", "application/msgpack")
            .body(serialize_msgpack(&request).unwrap())
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .unwrap();
        let response: WriteResponse = deserialize_msgpack(&response).unwrap();
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(
            response.conflicts.updated_instances.is_empty(),
            "{:?}",
            response.conflicts
        );
        assert!(!session.path().join("src/foo.lua").exists());
        let new_path = session.path().join("src/Destination/Moved.lua");
        assert!(new_path.is_file());

        // Reading the socket confirms structural changes are updates, not replacements.
        let packet = session
            .get_api_socket_packet(SocketPacketType::Messages, cursor)
            .unwrap();
        let SocketPacketBody::Messages(body) = packet.body else {
            panic!("Expected a messages packet");
        };
        assert!(body.messages.iter().all(|message| {
            !message.removed.contains(&source) && !message.added.contains_key(&source)
        }));
        let moved = session.get_api_read(root).unwrap();
        assert_eq!(moved.instances[&source].parent, destination);
        assert_eq!(moved.instances[&source].name, "Moved");

        // A subsequent real disk edit must reach the same managed instance, including
        // when the OS reports the earlier relocation notifications late.
        fs::write(&new_path, "return 739\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = session.get_api_read(root).unwrap();
            let instance = response
                .instances
                .get(&source)
                .expect("A filesystem echo must not replace the moved instance");
            assert_eq!(instance.parent, destination);
            if instance.properties.get(&"Source".into())
                .is_some_and(|value| matches!(value.as_ref(), Variant::String(source) if source == "return 739\n")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "The moved source did not receive its disk edit"
            );
            thread::sleep(Duration::from_millis(20));
        }
    });
}

#[test]
fn studio_move_promotes_script_parent_through_real_watcher_events() {
    use librojo::web_api::{InstanceUpdate, WriteAuthority, WriteRequest, WriteResponse};
    use rbx_dom_weak::{types::Variant, UstrMap};
    use std::{
        collections::HashMap,
        thread,
        time::{Duration, Instant},
    };

    run_serve_test("scripts", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let initial = session.get_api_read(root).unwrap();
        let child = initial
            .instances
            .values()
            .find(|instance| instance.name == "foo")
            .unwrap()
            .id;
        let parent = initial
            .instances
            .values()
            .find(|instance| instance.name == "bar")
            .unwrap()
            .id;
        let parent_bytes = fs::read(session.path().join("src/bar.server.lua")).unwrap();
        let request = WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(initial.message_cursor),
            authority: WriteAuthority::Studio,
            removed: Vec::new(),
            added: HashMap::new(),
            updated: vec![InstanceUpdate {
                id: child,
                changed_parent: Some(parent),
                changed_name: Some("Nested".into()),
                changed_class_name: None,
                changed_metadata: None,
                changed_properties: UstrMap::default(),
            }],
        };
        let response = reqwest::blocking::Client::new()
            .post(format!("http://localhost:{}/api/write", session.port()))
            .header("content-type", "application/msgpack")
            .body(serialize_msgpack(&request).unwrap())
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .unwrap();
        let response: WriteResponse = deserialize_msgpack(&response).unwrap();
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert!(
            response.conflicts.updated_instances.is_empty(),
            "{:?}",
            response.conflicts
        );
        assert!(!session.path().join("src/bar.server.lua").exists());
        assert!(!session.path().join("src/foo.lua").exists());
        let initializer = session.path().join("src/bar/init.server.lua");
        let nested = session.path().join("src/bar/Nested.lua");
        assert_eq!(fs::read(&initializer).unwrap(), parent_bytes);
        assert!(nested.is_file());
        fs::write(&initializer, "print('promoted parent')\n").unwrap();
        fs::write(&nested, "return 'promoted child'\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = session.get_api_read(root).unwrap();
            let parent_instance = response
                .instances
                .get(&parent)
                .expect("Promotion must retain the parent ID");
            let child_instance = response
                .instances
                .get(&child)
                .expect("Promotion must retain the child ID");
            assert_eq!(child_instance.parent, parent);
            assert_eq!(child_instance.name, "Nested");
            let parent_updated = parent_instance.properties.get(&"Source".into()).is_some_and(|value| matches!(value.as_ref(), Variant::String(source) if source == "print('promoted parent')\n"));
            let child_updated = child_instance.properties.get(&"Source".into()).is_some_and(|value| matches!(value.as_ref(), Variant::String(source) if source == "return 'promoted child'\n"));
            if parent_updated && child_updated {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Promoted instances did not receive later disk edits"
            );
            thread::sleep(Duration::from_millis(20));
        }
    });
}

#[test]
fn studio_move_cannot_overwrite_an_occupied_destination() {
    use librojo::web_api::{InstanceUpdate, WriteAuthority, WriteRequest, WriteResponse};
    use rbx_dom_weak::UstrMap;
    use std::{
        collections::HashMap,
        thread,
        time::{Duration, Instant},
    };

    run_serve_test("scripts", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let root = info.root_instance_id;
        let initial = session.get_api_read(root).unwrap();
        let source = initial
            .instances
            .values()
            .find(|instance| instance.name == "foo")
            .unwrap()
            .id;
        let original = fs::read(session.path().join("src/foo.lua")).unwrap();
        fs::create_dir(session.path().join("src/Destination")).unwrap();
        fs::write(
            session.path().join("src/Destination/foo.lua"),
            "return 'occupied'\n",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (destination, cursor) =
            loop {
                let response = session.get_api_read(root).unwrap();
                if let Some(instance) = response.instances.values().find(|instance| {
                    instance.name == "Destination" && !instance.children.is_empty()
                }) {
                    break (instance.id, response.message_cursor);
                }
                assert!(
                    Instant::now() < deadline,
                    "The watcher did not observe the occupied destination"
                );
                thread::sleep(Duration::from_millis(20));
            };
        let request = WriteRequest {
            session_id: info.session_id,
            message_cursor: Some(cursor),
            authority: WriteAuthority::Studio,
            removed: Vec::new(),
            added: HashMap::new(),
            updated: vec![InstanceUpdate {
                id: source,
                changed_parent: Some(destination),
                changed_name: None,
                changed_class_name: None,
                changed_metadata: None,
                changed_properties: UstrMap::default(),
            }],
        };
        let response = reqwest::blocking::Client::new()
            .post(format!("http://localhost:{}/api/write", session.port()))
            .header("content-type", "application/msgpack")
            .body(serialize_msgpack(&request).unwrap())
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .unwrap();
        let response: WriteResponse = deserialize_msgpack(&response).unwrap();
        assert!(!response.errors.is_empty());
        assert!(response.accepted.updated_instances.is_empty());
        assert!(response
            .conflicts
            .updated_instances
            .iter()
            .any(|update| update.id == source && update.changed_parent == Some(root)));
        assert_eq!(
            fs::read(session.path().join("src/foo.lua")).unwrap(),
            original
        );
        assert_eq!(
            fs::read_to_string(session.path().join("src/Destination/foo.lua")).unwrap(),
            "return 'occupied'\n"
        );
        assert_eq!(
            session.get_api_read(root).unwrap().instances[&source].parent,
            root
        );
    });
}

#[test]
fn rejects_dns_rebinding_requests() {
    run_serve_test("empty", |session, _redactions| {
        let port = session.port();
        let local_host = format!("localhost:{port}");

        // A request carrying a local Host header is served normally.
        assert_eq!(
            session
                .api_rojo_response_with_headers(&[("host", &local_host)])
                .status(),
            reqwest::StatusCode::OK,
        );

        // A request whose Host is a foreign hostname, as a DNS-rebound page
        // would send, is rejected with a generic 404 that reveals nothing about
        // the server.
        assert_rejected(session.api_rojo_response_with_headers(&[("host", "evil.com")]));

        // Even with a local Host, a present-but-foreign Origin is rejected.
        let foreign_origin = format!("http://evil.com:{port}");
        assert_rejected(
            session.api_rojo_response_with_headers(&[
                ("host", &local_host),
                ("origin", &foreign_origin),
            ]),
        );
    });
}

/// Asserts that a Host/Origin rejection is a generic 404 whose body and
/// content-type do not identify the server as Rojo.
fn assert_rejected(response: reqwest::blocking::Response) {
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        !content_type.contains("msgpack"),
        "rejection should not use the msgpack API content-type, got {content_type:?}",
    );

    let body = response.text().expect("Failed to read response body");
    let body_lower = body.to_lowercase();
    assert!(
        !body_lower.contains("rojo") && !body_lower.contains("rebinding"),
        "rejection body should not identify the server, got {body:?}",
    );
}

#[test]
fn allows_api_open_from_loopback_peer() {
    run_serve_test("empty", |session, _redactions| {
        // The harness always connects over loopback, so the local-only gate on
        // /api/open must let the request through. A bogus instance id then fails
        // id parsing with 400, which confirms we got past the gate rather than
        // being rejected with 403.
        assert_eq!(
            session.api_open_status("not-a-real-ref"),
            reqwest::StatusCode::BAD_REQUEST,
        );
    });
}

#[test]
fn empty() {
    run_serve_test("empty", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("empty_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn scripts() {
    run_serve_test("scripts", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("scripts_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        with_settings!({ sort_maps => true }, {
            assert_yaml_snapshot!(
                "scripts_all",
                read_response.intern_and_redact(&mut redactions, root_id)
            );
        });

        fs::write(session.path().join("src/foo.lua"), "Updated foo!").unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "scripts_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        with_settings!({ sort_maps => true }, {
            assert_yaml_snapshot!(
                "scripts_all-2",
                read_response.intern_and_redact(&mut redactions, root_id)
            );
        });
    });
}

#[test]
fn add_folder() {
    run_serve_test("add_folder", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("add_folder_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_folder_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::create_dir(session.path().join("src/my-new-folder")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "add_folder_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_folder_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn remove_file() {
    run_serve_test("remove_file", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("remove_file_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "remove_file_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::remove_file(session.path().join("src/hello.txt")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "remove_file_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "remove_file_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn edit_init() {
    run_serve_test("edit_init", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("edit_init_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "edit_init_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(session.path().join("src/init.lua"), b"-- Edited contents").unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "edit_init_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "edit_init_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

/// Folders don't track init files that don't exist yet, so creating one must
/// still reach the folder through its nearest tracked ancestor path.
#[test]
fn add_init_to_folder() {
    run_serve_test("add_init_to_folder", |session, _redactions| {
        let root_id = session.get_api_rojo().unwrap().root_instance_id;
        let folder_state = || {
            let read_response = session.get_api_read(root_id).unwrap();
            let folder = read_response
                .instances
                .values()
                .find(|instance| instance.name == "Folder")
                .expect("the folder instance should exist");
            let child_names: Vec<String> = folder
                .children
                .iter()
                .map(|id| read_response.instances[id].name.to_string())
                .collect();
            (folder.class_name.to_string(), child_names)
        };

        assert_eq!(
            folder_state(),
            ("Folder".to_owned(), vec!["Child".to_owned()])
        );

        fs::write(session.path().join("src/Folder/init.luau"), b"return 1").unwrap();
        session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();

        assert_eq!(
            folder_state(),
            ("ModuleScript".to_owned(), vec!["Child".to_owned()])
        );
    });
}

#[test]
fn move_folder_of_stuff() {
    run_serve_test("move_folder_of_stuff", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("move_folder_of_stuff_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        // Create a directory full of stuff we can move in
        let src_dir = tempdir().unwrap();
        let stuff_path = src_dir.path().join("new-stuff");

        fs::create_dir(&stuff_path).unwrap();

        // Make a bunch of random files in our stuff folder
        for i in 0..10 {
            let file_name = stuff_path.join(format!("{}.txt", i));
            let file_contents = format!("File #{}", i);

            fs::write(file_name, file_contents).unwrap();
        }

        // We're hoping that this rename gets picked up as one event. This test
        // will fail otherwise.
        fs::rename(stuff_path, session.path().join("src/new-stuff")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn empty_json_model() {
    run_serve_test("empty_json_model", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("empty_json_model_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(
            session.path().join("src/test.model.json"),
            r#"{"ClassName": "Model"}"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
#[ignore = "Rojo does not watch missing, optional files for changes."]
fn add_optional_folder() {
    run_serve_test("add_optional_folder", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("add_optional_folder", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::create_dir(session.path().join("create-later")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_alone() {
    run_serve_test("sync_rule_alone", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("sync_rule_alone_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_alone_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_complex() {
    run_serve_test("sync_rule_complex", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("sync_rule_complex_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_complex_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_no_extension() {
    run_serve_test("sync_rule_no_extension", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "sync_rule_no_extension_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_no_extension_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_default_project() {
    run_serve_test("no_name_default_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "no_name_default_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_default_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_project() {
    run_serve_test("no_name_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("no_name_project_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_top_level_project() {
    run_serve_test("no_name_top_level_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "no_name_top_level_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_top_level_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let project_path = session.path().join("default.project.json");
        let mut project_contents = fs::read_to_string(&project_path).unwrap();
        project_contents.push('\n');
        fs::write(&project_path, project_contents).unwrap();

        // The cursor shouldn't be changing so this snapshot is fine for testing
        // the response.
        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_top_level_project_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_no_name_project() {
    run_serve_test("sync_rule_no_name_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "sync_rule_no_name_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_no_name_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn ref_properties() {
    run_serve_test("ref_properties", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("ref_properties_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(
            session.path().join("ModelTarget.model.json"),
            r#"{
                "className": "Folder",
                "attributes": {
                    "Rojo_Id": "model target 2"
                },
                "children": [
                  {
                    "name": "ModelPointer",
                    "className": "Model",
                    "attributes": {
                      "Rojo_Target_PrimaryPart": "model target 2"
                    }
                  },
                  {
                    "name": "ProjectPointer",
                    "className": "Model",
                    "attributes": {
                      "Rojo_Target_PrimaryPart": "project target"
                    }
                  }
                ]
              }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "ref_properties_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn ref_properties_remove() {
    run_serve_test("ref_properties_remove", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("ref_properties_remove_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_remove_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::remove_file(session.path().join("src/target.model.json")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "ref_properties_remove_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_remove_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

/// When Ref properties were first implemented, a mistake was made that resulted
/// in Ref properties defined via attributes not being included in patch
/// computation, which resulted in subsequent patches setting those properties
/// to `nil`.
///
/// See: https://github.com/rojo-rbx/rojo/issues/929
#[test]
fn ref_properties_patch_update() {
    // Reusing ref_properties is fun and easy.
    run_serve_test("ref_properties", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "ref_properties_patch_update_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let target_path = session.path().join("ModelTarget.model.json");

        // Inserting scale just to force the change processor to run
        fs::write(
            target_path,
            r#"{
            "id": "model target",
            "className": "Folder",
            "children": [
                {
                    "name": "ModelPointer",
                    "className": "Model",
                    "attributes": {
                        "Rojo_Target_PrimaryPart": "model target"
                    },
                    "properties": {
                        "Scale": 1
                    }
                }
            ]
        }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn model_pivot_migration() {
    run_serve_test("pivot_migration", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("pivot_migration_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "pivot_migration_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let project_path = session.path().join("default.project.json");

        fs::write(
            project_path,
            r#"{
            "name": "pivot_migration",
            "tree": {
                "$className": "DataModel",
                "Workspace": {
                    "Model": {
                        "$className": "Model"
                    },
                    "Tool": {
                        "$path": "Tool.model.json"
                    },
                    "Actor": {
                        "$className": "Actor"
                    }
                }
            }
        }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "model_pivot_migration_all",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "model_pivot_migration_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn meshpart_with_id() {
    run_serve_test("meshpart_with_id", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("meshpart_with_id_info", redactions.redacted_yaml(&info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "meshpart_with_id_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        // This is a bit awkward, but it's fine.
        let (meshpart, _) = read_response
            .instances
            .iter()
            .find(|(_, inst)| inst.class_name == "MeshPart")
            .unwrap();
        let (objectvalue, _) = read_response
            .instances
            .iter()
            .find(|(_, inst)| inst.class_name == "ObjectValue")
            .unwrap();

        let body = session
            .post_api_serialize(&[*meshpart, *objectvalue], info.session_id)
            .unwrap()
            .bytes()
            .unwrap();
        let serialize_response: SerializeResponse =
            deserialize_msgpack(&body).expect("Server returned malformed response");

        // We don't assert a snapshot on the SerializeResponse because the model includes the
        // Refs from the DOM as names, which means it will obviously be different every time
        // this code runs. Still, we ensure that the SessionId is right at least.
        assert_eq!(serialize_response.session_id, info.session_id);

        let model = serialize_to_xml_model(&serialize_response, &redactions);
        assert_snapshot!("meshpart_with_id_serialize_model", model);
    });
}

#[test]
fn serialize_missing_id() {
    run_serve_test("empty", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let missing_id = Ref::new();

        let response = session
            .post_api_serialize(&[missing_id], info.session_id)
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    });
}

#[test]
fn forced_parent() {
    run_serve_test("forced_parent", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("forced_parent_info", redactions.redacted_yaml(&info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "forced_parent_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let body = session
            .post_api_serialize(&[root_id], info.session_id)
            .unwrap()
            .bytes()
            .unwrap();
        let serialize_response: SerializeResponse =
            deserialize_msgpack(&body).expect("Server returned malformed response");

        assert_eq!(serialize_response.session_id, info.session_id);

        let model = serialize_to_xml_model(&serialize_response, &redactions);
        assert_snapshot!("forced_parent_serialize_model", model);
    });
}

mod serve_lifetime {
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream},
        path::Path,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use hyper_tungstenite::tungstenite::client;

    use crate::rojo_test::{
        io_util::{KillOnDrop, ROJO_PATH, SERVE_TESTS_PATH},
        serve_util::read_server_port,
    };

    struct LifetimeServer {
        process: KillOnDrop,
        port: usize,
    }

    impl LifetimeServer {
        fn start(shutdown_on_stdin_eof: bool, stdin: Stdio) -> Self {
            let mut command = Command::new(ROJO_PATH);
            command
                .args(["--color", "never", "serve"])
                .arg(Path::new(SERVE_TESTS_PATH).join("empty"))
                .args(["--address", "127.0.0.1", "--port", "0"])
                .stdin(stdin)
                .stdout(Stdio::piped());
            if shutdown_on_stdin_eof {
                command.arg("--shutdown-on-stdin-eof");
            }
            let mut process = KillOnDrop(command.spawn().expect("Could not start Rojo"));
            let port = read_server_port(process.0.stdout.take().unwrap(), "Port:");
            Self { process, port }
        }

        fn assert_serving(&mut self) {
            assert!(self.process.0.try_wait().unwrap().is_none());
            let response = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap()
                .get(format!("http://127.0.0.1:{}/api/rojo", self.port))
                .send()
                .expect("Server did not respond");
            assert!(response.status().is_success());
            assert!(!response.bytes().unwrap().is_empty());
        }

        fn close_stdin(&mut self) {
            drop(self.process.0.stdin.take().expect("Stdin was not piped"));
        }

        fn assert_exited(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.process.0.try_wait().unwrap() {
                    assert!(status.success(), "Server failed with {status}");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Server did not stop after stdin EOF"
                );
                thread::sleep(Duration::from_millis(10));
            }
            // Check the listener without rebinding a port that may still have TIME_WAIT sockets.
            let address = SocketAddr::from(([127, 0, 0, 1], self.port as u16));
            assert!(
                TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err(),
                "Exited server still accepts connections"
            );
        }

        fn connect(&self) -> TcpStream {
            let stream = TcpStream::connect(("127.0.0.1", self.port as u16)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
        }
    }

    impl Drop for LifetimeServer {
        fn drop(&mut self) {
            // Only reap the exact subprocess this test owns, including on assertion failure.
            let _ = self.process.0.kill();
            let _ = self.process.0.wait();
        }
    }

    fn assert_connection_closed(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        if let Err(error) = stream.read_to_end(&mut bytes) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ),
                "Connection remained open or failed unexpectedly {error}"
            );
        }
    }

    #[test]
    fn shutdown_on_stdin_eof_ignores_bytes_until_the_pipe_closes() {
        let mut server = LifetimeServer::start(true, Stdio::piped());
        server.assert_serving();
        let input = server.process.0.stdin.as_mut().unwrap();
        input.write_all(b"keep running\n\x04\0\xff").unwrap();
        input.write_all(&[b'x'; 8192]).unwrap();
        input.flush().unwrap();
        thread::sleep(Duration::from_millis(250));
        server.assert_serving();

        server.close_stdin();
        server.assert_exited();
    }

    #[test]
    fn shutdown_on_stdin_eof_handles_initially_closed_stdin() {
        let mut server = LifetimeServer::start(true, Stdio::null());
        server.assert_exited();
    }

    #[test]
    fn serve_without_shutdown_on_stdin_eof_survives_closed_stdin() {
        for initially_closed in [false, true] {
            let mut server = LifetimeServer::start(
                false,
                if initially_closed {
                    Stdio::null()
                } else {
                    Stdio::piped()
                },
            );
            server.assert_serving();
            if !initially_closed {
                server.close_stdin();
            }
            thread::sleep(Duration::from_millis(250));
            server.assert_serving();
        }
    }

    #[test]
    fn shutdown_on_stdin_eof_drops_active_clients_without_stopping_another_server() {
        let mut server = LifetimeServer::start(true, Stdio::piped());
        let mut other = LifetimeServer::start(true, Stdio::piped());
        server.assert_serving();
        other.assert_serving();
        assert_ne!(server.port, other.port);

        let mut http = server.connect();
        write!(
            http,
            "POST /api/write HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 1000000\r\n\r\npartial body",
            server.port
        )
        .unwrap();
        http.flush().unwrap();
        let (mut websocket, response) = client(
            format!("ws://127.0.0.1:{}/api/socket/0", server.port),
            server.connect(),
        )
        .unwrap();
        assert_eq!(response.status().as_u16(), 101);
        // A completed request also gives the server time to accept the partial HTTP body.
        server.assert_serving();

        server.close_stdin();
        // Keep both clients open through the deadline, with no WebSocket close handshake.
        server.assert_exited();
        assert_connection_closed(&mut http);
        assert_connection_closed(websocket.get_mut());
        other.assert_serving();
        other.close_stdin();
        other.assert_exited();
    }
}
