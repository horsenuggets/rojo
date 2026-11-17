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
                    let SocketPacketBody::Messages(packet) = packet.body;
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
        let SocketPacketBody::Messages(packet) = packet.body;
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
    let SocketPacketBody::Messages(packet) = packet.body;
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

        for message in settled_messages(&session, fixture.cursor) {
            assert!(message.removed.is_empty(), "{:?}", message.removed);
            assert!(message.added.is_empty(), "{:?}", message.added.keys());
            for update in &message.updated {
                assert!(
                    update.changed_properties.is_empty(),
                    "re-reading the model reported {:?}",
                    update.changed_properties.keys()
                );
            }
        }
        let weld = fixture.id("Weld");
        assert_eq!(read_ref(&session, weld, "Part0"), Some(fixture.id("A")));
        assert_eq!(read_ref(&session, weld, "Part1"), Some(fixture.id("B")));
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

// A file can only hold references between its own instances. Moving what a
// reference points at out of its file is saved, and the reference that was
// lost is reported.
#[test]
fn studio_references_leaving_their_file_are_reported() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let pointer = fixture.id("Pointer");
        assert_eq!(
            read_ref(&session, pointer, "Value"),
            Some(fixture.id("Base"))
        );
        let response = fixture.write(
            &session,
            Vec::new(),
            vec![InstanceUpdate {
                changed_parent: Some(fixture.id("ReplicatedStorage")),
                ..shared_update(fixture.id("Base"))
            }],
        );
        assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
        assert_eq!(response.errors[0].id, pointer);
        assert_eq!(response.errors[0].property.as_deref(), Some("Value"));
        assert!(session.path().join("src/ReplicatedStorage/Base").is_dir());
    });
}

// Deleting what a reference points at leaves it nil, like in Studio, and
// isn't an error.
#[test]
fn studio_deletions_inside_model_files_clear_references() {
    run_serve_test("shared_files", |session, _| {
        let fixture = SharedFiles::read(&session);
        let response = fixture.write(&session, vec![fixture.id("A")], Vec::new());
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        settled_messages(&session, fixture.cursor);
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
        let SocketPacketBody::Messages(setup) = setup.body;
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
        let SocketPacketBody::Messages(body) = packet.body;
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
