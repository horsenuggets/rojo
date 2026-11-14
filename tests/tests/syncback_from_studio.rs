use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use librojo::web_api::{
    ManifestEntry, SyncbackFinishRequest, SyncbackFinishResponse, SyncbackInfoResponse,
};
use rbx_dom_weak::{types::Variant, ustr, InstanceBuilder, WeakDom};
use reqwest::{blocking::Client, StatusCode};
use tempfile::tempdir;

use crate::rojo_test::{
    io_util::{KillOnDrop, ROJO_PATH},
    serve_util::{deserialize_msgpack, get_port_number, serialize_msgpack},
};

const PROJECT: &str = r#"{
  "name": "FromStudio",
  "tree": {
    "$className": "DataModel",
    "Lighting": {
      "$properties": {
        "Brightness": 3
      }
    },
    "ReplicatedStorage": {
      "$path": "src/ReplicatedStorage"
    },
    "Workspace": {
      "$path": "src/Workspace"
    }
  }
}
"#;

fn entry(service: &str, name: &str, class_name: &str) -> ManifestEntry {
    ManifestEntry {
        service: service.to_owned(),
        name: name.to_owned(),
        class_name: class_name.to_owned(),
    }
}

/// Builds what the plugin would upload, as the top-level instances of one model
/// buffer, along with its manifest.
fn studio_upload() -> (Vec<u8>, Vec<ManifestEntry>) {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.root_ref();
    let shared = dom.insert(
        root,
        InstanceBuilder::new("ModuleScript")
            .with_name("Shared")
            .with_property("Source", "return 1"),
    );
    let target = dom.insert(root, InstanceBuilder::new("Part").with_name("Target"));
    let pointer = dom.insert(
        root,
        InstanceBuilder::new("ObjectValue")
            .with_name("Pointer")
            .with_property("Value", Variant::Ref(target)),
    );

    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, &[shared, pointer, target]).unwrap();

    let manifest = vec![
        entry("ReplicatedStorage", "Shared", "ModuleScript"),
        entry("ReplicatedStorage", "Pointer", "ObjectValue"),
        // Studio skips some instances, like `Player` objects, without saying so.
        entry("Workspace", "Skipped", "Part"),
        entry("Workspace", "Target", "Part"),
    ];
    (bytes, manifest)
}

fn wait_for_info(client: &Client, base: &str, process: &mut KillOnDrop) -> SyncbackInfoResponse {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = process.0.try_wait().unwrap() {
            panic!("rojo syncback exited early with {status}");
        }
        if let Ok(response) = client.get(format!("{base}/api/syncback")).send() {
            return deserialize_msgpack(&response.bytes().unwrap()).unwrap();
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("rojo syncback --from-studio didn't start listening in time");
}

#[test]
fn syncback_from_studio_writes_the_uploaded_place() {
    let dir = tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap().join("project");
    fs_err::create_dir_all(project.join("src/ReplicatedStorage")).unwrap();
    fs_err::create_dir_all(project.join("src/Workspace")).unwrap();
    fs_err::write(project.join("default.project.json"), PROJECT).unwrap();

    let port = get_port_number().to_string();
    let mut process = KillOnDrop(
        Command::new(ROJO_PATH)
            .args(["--color", "never", "syncback"])
            .arg(&project)
            .args(["--from-studio", "--non-interactive", "--port", &port])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let client = Client::new();
    let base = format!("http://localhost:{port}");
    let info = wait_for_info(&client, &base, &mut process);
    assert_eq!(info.project_name, "FromStudio");
    let service_names: Vec<_> = info.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        service_names,
        ["Lighting", "ReplicatedStorage", "Workspace"]
    );

    // A plugin connecting as if this were `rojo serve` must not get a session,
    // since that would push the project into Studio.
    let serve = client.get(format!("{base}/api/rojo")).send().unwrap();
    assert_eq!(serve.status(), StatusCode::NOT_FOUND);

    let post_part = |upload_id: &str, index: usize, part: Vec<u8>| {
        client
            .post(format!(
                "{base}/api/syncback/part/{}/{upload_id}/{index}",
                info.syncback_id
            ))
            .body(part)
            .send()
            .unwrap()
            .status()
    };

    let wrong_id = client
        .post(format!("{base}/api/syncback/part/not-the-id/attempt/0"))
        .body(vec![0])
        .send()
        .unwrap();
    assert_eq!(wrong_id.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        post_part("attempt", 0, vec![0; info.part_size + 1]),
        StatusCode::BAD_REQUEST
    );

    // An earlier attempt failed partway, leaving a part with a higher index than
    // the retry uses. The retry must not trip over it.
    assert_eq!(
        post_part("failed-attempt", 7, vec![1, 2, 3]),
        StatusCode::OK
    );

    // Parts arrive out of order when the plugin uploads them in parallel.
    let (bytes, manifest) = studio_upload();
    let parts: Vec<_> = bytes.chunks(bytes.len().div_ceil(3)).collect();
    for (index, part) in parts.iter().enumerate().rev() {
        assert_eq!(post_part("retry", index, part.to_vec()), StatusCode::OK);
    }

    let finish = client
        .post(format!("{base}/api/syncback/finish"))
        .body(
            serialize_msgpack(SyncbackFinishRequest {
                syncback_id: info.syncback_id,
                upload_id: "retry".to_owned(),
                part_count: parts.len() as u32,
                manifest,
                current_camera: None,
            })
            .unwrap(),
        )
        .send()
        .unwrap();
    assert_eq!(finish.status(), StatusCode::OK);
    let finish: SyncbackFinishResponse = deserialize_msgpack(&finish.bytes().unwrap()).unwrap();
    // Three uploaded instances, plus the three services they were placed in.
    assert_eq!(finish.instance_count, 6);
    assert_eq!(finish.dropped, vec![entry("Workspace", "Skipped", "Part")]);

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "rojo syncback didn't exit");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());

    assert_eq!(
        fs_err::read_to_string(project.join("src/ReplicatedStorage/Shared.luau")).unwrap(),
        "return 1"
    );

    // Studio can't serialize services, so the project's service properties must
    // be left alone instead of being treated as removed.
    let project_file = fs_err::read_to_string(project.join("default.project.json")).unwrap();
    let project_json: serde_json::Value = serde_json::from_str(&project_file).unwrap();
    assert_eq!(
        project_json["tree"]["Lighting"]["$properties"]["Brightness"],
        3
    );

    // Building the synced project must recreate the reference from one service
    // into another.
    let built = dir.path().join("built.rbxl");
    let build = Command::new(ROJO_PATH)
        .args(["--color", "never", "build"])
        .arg(&project)
        .arg("--output")
        .arg(&built)
        .status()
        .unwrap();
    assert!(build.success());
    let dom = rbx_binary::from_reader(fs_err::read(&built).unwrap().as_slice()).unwrap();
    let pointer = dom
        .descendants()
        .find(|inst| inst.name == "Pointer")
        .expect("Pointer should be built");
    let Some(Variant::Ref(value)) = pointer.properties.get(&ustr("Value")) else {
        panic!("Pointer.Value should be a reference");
    };
    assert_eq!(dom.full_path_of(*value, "."), "Workspace.Target");
}
