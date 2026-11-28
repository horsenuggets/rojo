use std::{ffi::OsStr, path::Path, process::Command};

use insta::assert_snapshot;
use rbx_dom_weak::{types::Variant, ustr, WeakDom};

use crate::rojo_test::{
    io_util::{ROJO_PATH, SYNCBACK_TESTS_PATH},
    syncback_util::{run_syncback_test, snapshot_rbxm},
};

#[test]
fn package_sources_cli_build_sourcemap_and_round_trip() {
    use crate::rojo_test::package_sources::{self as fixture, ORIGINAL};
    use std::fs;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fixture::setup(&root);
    let output = temp.path().join("input.rbxl");
    let build = || {
        let result = Command::new(ROJO_PATH)
            .arg("build")
            .arg(&root)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap()
    };
    let mut dom = build();
    let local = dom
        .descendants()
        .find(|inst| inst.name == "foo")
        .unwrap()
        .referent();
    assert_eq!(
        dom.get_by_ref(local).unwrap().properties[&ustr("Source")],
        Variant::String("return { Value = 1 }\n".into())
    );
    let app = dom.descendants().find(|inst| inst.name == "App").unwrap();
    assert!(
        matches!(&app.properties[&ustr("Source")], Variant::String(source) if source.contains("@game/ReplicatedStorage/Shared/Util"))
    );
    assert!(!dom.descendants().any(|inst| inst.name == "ForkedPackages"));
    let map = Command::new(ROJO_PATH)
        .arg("sourcemap")
        .arg(&root)
        .arg("--include-non-scripts")
        .output()
        .unwrap();
    assert!(
        map.status.success(),
        "{}",
        String::from_utf8_lossy(&map.stderr)
    );
    let map: serde_json::Value = serde_json::from_slice(&map.stdout).unwrap();
    fn local_node(value: &serde_json::Value) -> Option<&serde_json::Value> {
        if value["name"] == "foo" {
            return Some(value);
        }
        value["children"].as_array()?.iter().find_map(local_node)
    }
    let paths = local_node(&map).unwrap()["filePaths"].as_array().unwrap();
    assert_eq!(paths[0], ORIGINAL);
    assert!(paths.iter().any(|path| path
        .as_str()
        .unwrap()
        .contains("Packages/_Index/scope_foo@local/foo/Source/Foo/init.luau")));
    // A normal source edit is authoritative without changing any manifest hash.
    fs::write(root.join(ORIGINAL), "return { Value = 2 }\n").unwrap();
    dom = build();
    let local = dom
        .descendants()
        .find(|inst| inst.name == "foo")
        .unwrap()
        .referent();
    assert_eq!(
        dom.get_by_ref(local).unwrap().properties[&ustr("Source")],
        Variant::String("return { Value = 2 }\n".into())
    );
    let before = fixture::files(&root);
    dom.get_by_ref_mut(local).unwrap().properties.insert(
        ustr("Source"),
        Variant::String("return { Value = 3 }\n".into()),
    );
    rbx_binary::to_writer(
        fs::File::create(&output).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    let result = Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(&root)
        .arg("--input")
        .arg(&output)
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut expected = before;
    expected.insert(ORIGINAL.into(), b"return { Value = 3 }\n".to_vec());
    assert_eq!(fixture::files(&root), expected);
}

#[test]
fn package_sources_cli_syncback_preserves_selected_inner_project_file() {
    package_sources_cli_syncback_preserves_selected_inner_project(
        "inner.project.json",
        "Source/Foo",
    );
}

#[test]
fn package_sources_cli_syncback_preserves_selected_inner_project_directory() {
    package_sources_cli_syncback_preserves_selected_inner_project(
        "inner/default.project.json",
        "../Source/Foo",
    );
}

#[test]
fn package_sources_cli_syncback_preserves_selected_inner_project_directory_jsonc() {
    package_sources_cli_syncback_preserves_selected_inner_project(
        "inner/default.project.jsonc",
        "../Source/Foo",
    );
}

fn package_sources_cli_syncback_preserves_selected_inner_project(
    inner_project: &str,
    inner_source: &str,
) {
    use crate::rojo_test::package_sources::{self as fixture, INSTALLED, ORIGINAL};
    use std::{
        fs,
        io::{Read, Seek},
        thread,
        time::{Duration, Instant},
    };

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fixture::setup(&root);
    // Both projects share one instance. The outer project must not replace the
    // inner project selected during syncback, even on a transparent root.
    for package in ["Source/ForkedPackages/foo", INSTALLED] {
        let inner_path = if inner_project.starts_with("inner/") {
            "inner"
        } else {
            inner_project
        };
        fs::write(
            root.join(package).join("default.project.json"),
            serde_json::json!({"tree": {"$path": inner_path}}).to_string(),
        )
        .unwrap();
        let inner_project = root.join(package).join(inner_project);
        fs::create_dir_all(inner_project.parent().unwrap()).unwrap();
        fs::write(
            inner_project,
            serde_json::json!({"tree": {"$path": inner_source}}).to_string(),
        )
        .unwrap();
    }
    let output = temp.path().join("input.rbxl");
    let result = Command::new(ROJO_PATH)
        .arg("build")
        .arg(&root)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut dom = rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap();
    let local = dom
        .descendants()
        .find(|inst| inst.name == "foo")
        .unwrap()
        .referent();
    dom.get_by_ref_mut(local).unwrap().properties.insert(
        ustr("Source"),
        Variant::String("return { Value = 3 }\n".into()),
    );
    rbx_binary::to_writer(
        fs::File::create(&output).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    let mut expected = fixture::files(&root);
    expected.insert(ORIGINAL.into(), b"return { Value = 3 }\n".to_vec());

    // Capture diagnostics in a file so a looping child cannot fill a pipe and
    // block the test. Always kill and reap the child before reporting a timeout.
    let stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(&root)
        .arg("--input")
        .arg(&output)
        .arg("--non-interactive")
        .stdout(std::process::Stdio::null())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Package syncback exceeded its 10-second deadline");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = stderr;
    stderr.rewind().unwrap();
    let mut diagnostics = String::new();
    stderr.read_to_string(&mut diagnostics).unwrap();
    assert!(status.success(), "{diagnostics}");
    // This also pins the unchanged outer/inner projects and all installed bytes.
    assert_eq!(fixture::files(&root), expected);
}

#[test]
fn package_sources_private_aliases_do_not_use_the_consumers_namespace() {
    use crate::rojo_test::package_sources::{self as fixture, ORIGINAL};
    use sha2::{Digest, Sha256};
    use std::fs;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fixture::setup(&root);
    let authored = "local dependency = require(\"@packages/Util\")\nlocal helper = require(\"@shared/Util\")\nreturn dependency, helper, 1\n";
    fs::write(root.join(ORIGINAL), authored).unwrap();
    let manifest_path = "Source/ForkedPackages/foo/wally.toml";
    fs::write(
        root.join(manifest_path),
        "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n[dependencies]\nUtil = \"scope/private@1.0.0\"\n",
    )
    .unwrap();
    let root_manifest = root.join("wally.toml");
    fs::write(
        &root_manifest,
        format!(
            "{}Util = \"scope/registry@1.2.3\"\n",
            fs::read_to_string(&root_manifest).unwrap()
        ),
    )
    .unwrap();
    for (path, contents) in [
        (
            ".luaurc",
            r#"{"aliases":{"Packages":"Packages","shared":"Source/Shared"}}"#,
        ),
        (
            "Packages/Util.luau",
            "return require(\"./_Index/scope_registry@1.2.3/registry\")\n",
        ),
        (
            "Packages/_Index/scope_foo@local/Util.luau",
            "return require(\"../scope_private@1.0.0/private\")\n",
        ),
        (
            "Packages/_Index/scope_foo@local/.luaurc",
            r#"{"aliases":{"packages":"./"}}"#,
        ),
        (
            "Packages/_Index/scope_private@1.0.0/private/init.luau",
            "return 99\n",
        ),
        ("Source/App.luau", "return require(\"@packages/Util\")\n"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let metadata_path = root.join("wally.install.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata["manifest_hash"] = serde_json::json!(format!(
        "{:x}",
        Sha256::digest(fs::read(&root_manifest).unwrap())
    ));
    metadata["local_manifests"][manifest_path] = serde_json::json!(format!(
        "{:x}",
        Sha256::digest(fs::read(root.join(manifest_path)).unwrap())
    ));
    metadata["package_roots"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "name":"scope/private", "kind":"registry", "realm":"shared",
            "install_path":"Packages/_Index/scope_private@1.0.0/private", "package_version":"1.0.0"
        }));
    metadata["generated_paths"].as_array_mut().unwrap().extend(
        [
            "Packages/Util.luau",
            "Packages/_Index/scope_foo@local/Util.luau",
            "Packages/_Index/scope_foo@local/.luaurc",
        ]
        .into_iter()
        .map(serde_json::Value::from),
    );
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    let output = temp.path().join("input.rbxl");
    let result = Command::new(ROJO_PATH)
        .arg("build")
        .arg(&root)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut dom = rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap();
    let local = dom
        .descendants()
        .find(|instance| instance.name == "foo")
        .unwrap()
        .referent();
    let app = dom
        .descendants()
        .find(|instance| instance.name == "App")
        .unwrap();
    assert_eq!(
        app.properties[&ustr("Source")],
        Variant::String("return require(\"@game/ReplicatedStorage/Packages/Util\")\n".into())
    );
    let Variant::String(emitted) = &dom.get_by_ref(local).unwrap().properties[&ustr("Source")]
    else {
        panic!("Missing source")
    };
    assert!(
        emitted.contains("@game/ReplicatedStorage/Packages/_Index/scope_foo@local/Util"),
        "{emitted}"
    );
    assert!(
        emitted.contains("@game/ReplicatedStorage/Shared/Util"),
        "{emitted}"
    );
    let edited = emitted.replace("helper, 1", "helper, 2");
    dom.get_by_ref_mut(local)
        .unwrap()
        .properties
        .insert(ustr("Source"), Variant::String(edited));
    let before = fixture::files(&root);
    rbx_binary::to_writer(
        fs::File::create(&output).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    let result = Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(&root)
        .arg("--input")
        .arg(&output)
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut expected = before;
    expected.insert(
        ORIGINAL.into(),
        authored.replace("helper, 1", "helper, 2").into_bytes(),
    );
    assert_eq!(fixture::files(&root), expected);
}

#[test]
fn package_sources_cli_registry_and_generated_differences_are_warning_only() {
    use crate::rojo_test::package_sources as fixture;
    use std::fs;
    for (target, mutation) in ["registry", "Foo"]
        .into_iter()
        .flat_map(|target| ["source", "rename", "delete"].map(move |mutation| (target, mutation)))
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fixture::setup(&root);
        let output = temp.path().join("input.rbxl");
        assert!(Command::new(ROJO_PATH)
            .arg("build")
            .arg(&root)
            .arg("--output")
            .arg(&output)
            .status()
            .unwrap()
            .success());
        let mut dom = rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap();
        let id = dom
            .descendants()
            .find(|inst| inst.name == target)
            .unwrap()
            .referent();
        match mutation {
            "source" => {
                dom.get_by_ref_mut(id).unwrap().properties.insert(
                    ustr("Source"),
                    Variant::String("return 'Studio difference'\n".into()),
                );
            }
            "rename" => dom.get_by_ref_mut(id).unwrap().name = "Renamed".into(),
            "delete" => dom.destroy(id),
            _ => unreachable!(),
        }
        rbx_binary::to_writer(
            fs::File::create(&output).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let before = fixture::files(&root);
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&root)
            .arg("--input")
            .arg(&output)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("read-only"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fixture::files(&root), before);
    }
}

#[test]
fn package_sources_cli_invalid_or_absent_provenance_fails_closed() {
    use crate::rojo_test::package_sources as fixture;
    use std::fs;
    for case in fixture::INVALID_INSTALL_CASES {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fixture::setup(&root);
        fixture::invalidate(&root, case);
        let output = temp.path().join("input.rbxl");
        let result = Command::new(ROJO_PATH)
            .arg("build")
            .arg(&root)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{case} {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut dom = rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap();
        let id = dom
            .descendants()
            .find(|inst| inst.name == "registry")
            .unwrap()
            .referent();
        dom.get_by_ref_mut(id)
            .unwrap()
            .properties
            .insert(ustr("Source"), Variant::String("return 'changed'\n".into()));
        rbx_binary::to_writer(
            fs::File::create(&output).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let before = fixture::files(&root);
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&root)
            .arg("--input")
            .arg(&output)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{case} {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("wally.install.json"));
        assert_eq!(fixture::files(&root), before, "{case}");
    }
}

#[test]
fn package_sources_reject_duplicate_original_or_installed_module_mounts() {
    use crate::rojo_test::package_sources as fixture;
    use std::fs;
    for path in [
        "Source/ForkedPackages/foo/Source/Foo/Child.luau",
        "Packages/_Index/scope_foo@local/foo/Source/Foo/Child.luau",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fixture::setup(&root);
        let project = root.join("place.project.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
        value["tree"]["ReplicatedStorage"]["Duplicate"] = serde_json::json!({"$path":path});
        fs::write(project, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = fixture::files(&root);
        let result = Command::new(ROJO_PATH)
            .arg("build")
            .arg(&root)
            .arg("--output")
            .arg(temp.path().join("output.rbxl"))
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("mounted more than once"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fixture::files(&root), before);
    }
}

#[test]
fn package_sources_cli_additions_and_renames_leave_installed_outputs_untouched() {
    use crate::rojo_test::package_sources as fixture;
    use std::fs;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fixture::setup(&root);
    let output = temp.path().join("input.rbxl");
    assert!(Command::new(ROJO_PATH)
        .arg("build")
        .arg(&root)
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap()
        .success());
    let mut dom = rbx_binary::from_reader(fs::File::open(&output).unwrap()).unwrap();
    let parent = dom
        .descendants()
        .find(|inst| inst.name == "foo")
        .unwrap()
        .referent();
    let child = dom
        .descendants()
        .find(|inst| inst.name == "Child")
        .unwrap()
        .referent();
    dom.get_by_ref_mut(child).unwrap().name = "Renamed".into();
    dom.insert(
        parent,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Added")
            .with_property("Source", "return 101\n"),
    );
    rbx_binary::to_writer(
        fs::File::create(&output).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    let mut expected = fixture::files(&root);
    let result = Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(&root)
        .arg("--input")
        .arg(&output)
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let contents = expected
        .remove("Source/ForkedPackages/foo/Source/Foo/Child.luau")
        .unwrap();
    expected.insert(
        "Source/ForkedPackages/foo/Source/Foo/Renamed.luau".into(),
        contents,
    );
    expected.insert(
        "Source/ForkedPackages/foo/Source/Foo/Added.luau".into(),
        b"return 101\n".to_vec(),
    );
    assert_eq!(fixture::files(&root), expected);
    let result = Command::new(ROJO_PATH)
        .arg("sourcemap")
        .arg(&root)
        .output()
        .unwrap();
    assert!(result.status.success());
    let map = String::from_utf8(result.stdout).unwrap();
    assert!(
        map.contains("Packages/_Index/scope_foo@local/foo/Source/Foo/Added.luau"),
        "A newly added source must keep its alternate import path before reinstall"
    );
}

macro_rules! syncback_tests {
    ($($test_name:ident => $list:expr$(,)?),*) => {$(
        #[test]
        fn $test_name() {
            run_syncback_test(stringify!($test_name), |path| {
                for name in $list {
                    let snapshot_name = format!(concat!(stringify!($test_name), "-{}"), name);
                    let new = path.join::<&str>(name);
                    if !new.exists() {
                        panic!("the path stub '{}' does not exist after syncback runs. consider double checking for typos.", name);
                    }
                    if let Some("rbxm") = new.extension().and_then(OsStr::to_str) {
                        let content = fs_err::read(new).unwrap();
                        snapshot_rbxm(&snapshot_name, content, name);
                    } else {
                        let content = fs_err::read_to_string(new).unwrap();
                        assert_snapshot!(snapshot_name, content, name);
                    }
                }
            });
        }
    )*};
}

syncback_tests! {
    // Ensures that there's only one copy written to disk if navigating a
    // project file might yield two copies
    child_but_not => ["OnlyOneCopy/child_of_one.luau", "ReplicatedStorage/child_replicated_storage.luau"],
    // Ensures that syncback works with CSVs
    csv => ["src/csv_init/init.csv", "src/csv.csv"],
    // Ensures that if a RojoId is duplicated somewhere in the project, it's
    // rewritten rather than synced back as a conflict
    duplicate_rojo_id => ["container.model.json"],
    // Ensures that the `ignorePaths` setting works for additions
    ignore_paths_adding => ["src/int_value.model.json", "src/subfolder/string_value.txt"],
    // Ensures that the `ignorePaths` setting works for `init` files
    ignore_paths_init => ["src/non-init.luau", "src/init-file/init.luau"],
    // Ensures that the `ignorePaths` setting works for removals
    ignore_paths_removing => ["src/Message.rbxm"],
    // Ensures that `ignoreTrees` works for additions
    ignore_trees_adding => [],
    // Ensures that `ignoreTrees` works for removals
    ignore_trees_removing => [],
    // Ensures that excluded paths keep a subtree out of the inline layout, so
    // neither the metadata nor a child file is lost or duplicated
    inline_ignore_paths => ["src/Props/init.meta.json", "src/Props/Keep/init.meta.json", "src/Meta/Child/init.meta.json"],
    // Ensures that an inline folder stored in init.meta.jsonc is rewritten in
    // place instead of gaining a second metadata file
    inline_jsonc_rewrite => ["src/Holder/init.meta.jsonc"],
    // Ensures that a project already matching the place still moves into the
    // inline layout, instead of being skipped as unchanged
    inline_relayout_unchanged => ["src/Holder/init.meta.json"],
    // Ensures that directories switch between inline children and a file for
    // each child as scripts leave or enter their subtree
    inline_layout_changes => ["src/Collapse/init.meta.json", "src/Expand/Holder/Added.legacy.luau", "src/Expand/Holder/Brick/init.meta.json"],
    // Ensures that script-free subtrees are written into one init.meta.json
    inline_script_free => ["src/Props/init.meta.json", "src/Mixed/Data/init.meta.json", "src/Mixed/Module.luau", "src/Mixed/Scripts/Marker/init.meta.json", "src/InPointer/init.meta.json"],
    // Ensures that all of the JSON middlewares are handled as expected
    json_middlewares => ["src/dir_with_meta/init.meta.json", "src/model_json.model.json", "src/project_json.project.json"],
    // Ensures that `modelTrees` rules write matching subtrees as model files
    // in the format they ask for, replace the directories they used to be
    // even when nothing changed, keep unsafe names in adjacent metadata, and
    // that a model file no rule matches goes back to the directory layout
    model_trees => ["src/Plain/Odd_Name.meta.json", "src/Plain/Odd_Name.rbxm", "src/Revert/init.meta.json", "src/Scenes/Intro/FX.rbxm", "src/Scenes/Outro/FX.rbxm", "src/Text/Legible.rbxmx"],
    // Ensures that a project already laid out by its `modelTrees` rules is
    // left untouched
    model_trees_stable => [],
    // Ensures projects that refer to other projects work as expected
    nested_projects => ["nested.project.json", "string_value.txt"],
    // Non-finite number attributes are kept in their tagged form by every JSON writer.
    non_finite_attributes => ["default.project.json", "src/Directory/init.meta.json", "src/Model/init.meta.json", "src/Model/Value.model.json"],
    // Ensures files that are ignored by nested projects are picked up if
    // they're included in second project. Unusual but perfectly workable
    // pattern that syncback has to support.
    nested_projects_weird => ["src/modules/ClientModule.luau", "src/modules/ServerModule.luau"],
    // Ensures that projects respect `init` files when they're directly referenced from a node
    project_init => ["src/init.luau"],
    // Ensures that projects can be reserialized by syncback and that
    // default.project.json doesn't change unexpectedly.
    project_reserialize => ["attribute_mismatch.luau", "property_mismatch.project.json"],
    // Confirms that Instances that cannot serialize as directories serialize as rbxms
    rbxm_fallback => ["src/ChildWithDuplicates.rbxm"],
    // Ensures that ref properties are linked properly on the file system
    ref_properties => ["src/pointer.model.json", "src/target.model.json"],
    // Ensures that ref properties are linked when no attributes are manually
    // set in the DataModel
    ref_properties_blank => ["src/pointer.model.json", "src/target.meta.json", "src/target.txt"],
    // Ensures that if there is a conflict in RojoRefs, one of them is rewritten.
    ref_properties_conflict => ["src/Pointer_2.model.json", "src/Target_2.model.json"],
    // Ensures that having multiple pointers that are aimed at the same target doesn't trigger ref rewrites.
    ref_properties_duplicate => [],
    // Ensures that ref properties that point to nothing after the prune both
    // do not leave any trace of themselves
    ref_properties_pruned => ["src/Pointer1.model.json", "src/Pointer2.model.json", "src/Pointer3.model.json"],
    // Ensures that the old middleware is respected during syncback
    respect_old_middleware => ["default.project.json", "src/model_json.model.json", "src/rbxm.rbxm", "src/rbxmx.rbxmx"],
    // Ensures that the `$schema` field roundtrips with syncback
    schema_roundtrip => ["default.project.json", "src/model.model.json", "src/init/init.meta.json", "src/adjacent.meta.json"],
    // Ensures that StringValues inside project files are written to the
    // project file, but only if they don't have `$path` set
    string_value_project => ["default.project.json"],
    // Ensures that sync rules are respected. This is really just a test to
    // ensure it uses the old path when possible, but we want the coverage.
    sync_rules => ["src/module.modulescript", "src/text.text"],
    // Ensures that the `syncUnscriptable` setting works
    unscriptable_properties => ["default.project.json"],
    // Ensures that syncback correctly removes default values from projects rather
    // than leaving an incorrect value.
    project_default_properties_remove => ["default.project.json"],
}

/// Describes every instance under `src` with the values this test cares about,
/// including where each reference points, so two trees can be compared.
fn describe_src(dom: &WeakDom) -> Vec<String> {
    let src = dom
        .descendants()
        .find(|instance| instance.name == "src")
        .expect("the tree should have a src folder")
        .referent();
    let mut lines = Vec::new();
    let mut pending = vec![src];
    while let Some(referent) = pending.pop() {
        let instance = dom.get_by_ref(referent).unwrap();
        let path = dom.full_path_of(referent, "/");
        let path = &path[path.find("src").unwrap()..];
        let mut line = format!("{path} ({})", instance.class);
        for name in ["Value", "Transparency", "Anchored"] {
            match instance.properties.get(&ustr(name)) {
                Some(Variant::Ref(target)) if target.is_some() => {
                    let target = dom.full_path_of(*target, "/");
                    line += &format!(" {name}->{}", &target[target.find("src").unwrap()..]);
                }
                Some(Variant::Ref(_)) | None => {}
                Some(value) => line += &format!(" {name}={value:?}"),
            }
        }
        lines.push(line);
        pending.extend(instance.children().iter().copied());
    }
    lines.sort();
    lines
}

/// Inline children are only useful if building the synced project recreates
/// the original instances, with references into, out of, and within them.
#[test]
fn inline_script_free_round_trip() {
    run_syncback_test("inline_script_free", |path| {
        let output = path.join("round_trip.rbxm");
        let status = Command::new(ROJO_PATH)
            .args(["--color", "never", "build"])
            .arg(path)
            .arg("--output")
            .arg(&output)
            .status()
            .unwrap();
        assert!(status.success());

        let built = rbx_binary::from_reader(fs_err::read(&output).unwrap().as_slice()).unwrap();
        let input_path = Path::new(SYNCBACK_TESTS_PATH)
            .join("inline_script_free")
            .join("input.rbxm");
        let input = rbx_binary::from_reader(fs_err::read(input_path).unwrap().as_slice()).unwrap();
        assert_eq!(describe_src(&built), describe_src(&input));
    });
}

/// Generated names must not alias NTFS short names, and metadata must retain
/// logical names and duplicate order through export, rebuild, and another export.
#[test]
fn short_name_collision_exports_round_trip_without_changes() {
    use rbx_dom_weak::InstanceBuilder;
    use std::collections::BTreeMap;

    #[derive(Debug, PartialEq)]
    struct Record {
        class: String,
        source: Option<Variant>,
        child_source: Variant,
    }

    fn records(dom: &WeakDom) -> BTreeMap<String, Vec<Record>> {
        let storage = dom
            .descendants()
            .find(|instance| instance.class == ustr("ReplicatedStorage"))
            .unwrap();
        let mut records: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for referent in storage.children() {
            let instance = dom.get_by_ref(*referent).unwrap();
            assert!(matches!(instance.class.as_str(), "Folder" | "ModuleScript"));
            assert_eq!(instance.children().len(), 1);
            let child = dom.get_by_ref(instance.children()[0]).unwrap();
            assert_eq!(child.name, "Child");
            assert_eq!(child.class, ustr("ModuleScript"));
            records
                .entry(instance.name.clone())
                .or_default()
                .push(Record {
                    class: instance.class.to_string(),
                    source: instance.properties.get(&ustr("Source")).cloned(),
                    child_source: child.properties[&ustr("Source")].clone(),
                });
        }
        records
    }

    for natural_class in ["Folder", "ModuleScript"] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs_err::create_dir_all(project.join("src")).unwrap();
        fs_err::write(
            project.join("default.project.json"),
            r#"{
  "name": "ShortNames",
  "syncbackRules": { "instancesAsDirectories": true },
  "tree": {
    "$className": "DataModel",
    "ReplicatedStorage": { "$path": "src" }
  }
}"#,
        )
        .unwrap();
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let storage = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
        let mut names = Vec::new();
        for name in ["bucket", "Flower", "FlowerAEL1", "Stone5"] {
            names.extend(std::iter::repeat_n((name, "ModuleScript"), 14));
        }
        // Natural suffixes collide with generated paths, while tildes collide with
        // sanitized names. Every Instance must survive with its original name.
        names.extend([
            ("bucket.__2", "ModuleScript"),
            ("bucket~2", "ModuleScript"),
            ("bucket_2", "ModuleScript"),
            ("Flower~1", "ModuleScript"),
            ("Stone5~2", "ModuleScript"),
            ("~", "ModuleScript"),
            ("Natural__10", "ModuleScript"),
            ("Natural__10", "ModuleScript"),
            ("Natural.__10", natural_class),
            ("Natural.__10", natural_class),
            ("Odd?.__10", "ModuleScript"),
            ("Odd?.__10", "ModuleScript"),
            ("Part", natural_class),
            ("Part", natural_class),
            ("Part.__2", natural_class),
            ("Part.__2", natural_class),
            ("Part__2", natural_class),
            ("Part__2", natural_class),
        ]);
        for (index, (name, class)) in names.iter().enumerate() {
            let mut builder = InstanceBuilder::new(*class).with_name(*name).with_child(
                InstanceBuilder::new("ModuleScript")
                    .with_name("Child")
                    .with_property("Source", format!("return {}", index + 100)),
            );
            if *class == "ModuleScript" {
                builder = builder.with_property("Source", format!("return {index}"));
            }
            dom.insert(storage, builder);
        }
        let input = dir.path().join("input.rbxl");
        rbx_binary::to_writer(fs_err::File::create(&input).unwrap(), &dom, &[storage]).unwrap();
        let sync = |input: &Path| {
            let result = Command::new(ROJO_PATH)
                .args(["--color", "never", "syncback"])
                .arg(&project)
                .arg("--input")
                .arg(input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            read_files(&project)
        };
        let first = sync(&input);
        assert!(first.keys().all(|path| !path.contains('~')));
        for name in ["bucket", "Flower", "FlowerAEL1", "Stone5"] {
            for number in 1..=14 {
                let stem = if number == 1 {
                    name.to_owned()
                } else {
                    format!("{name}.__{number}")
                };
                assert!(first.contains_key(&format!("src/{stem}/init.luau")));
            }
        }
        let natural_stems = if natural_class == "Folder" {
            ["Part.__2.__2", "Part.__2.__3"]
        } else {
            ["Part___2", "Part___2.__2"]
        };
        for (stem, name) in [
            ("bucket___2", "bucket.__2"),
            ("bucket_2", "bucket~2"),
            ("bucket_2.__2", "bucket_2"),
            ("Part.__2", "Part"),
            (natural_stems[0], "Part.__2"),
            (natural_stems[1], "Part.__2"),
            ("Part__2.__2", "Part__2"),
            ("Flower_1", "Flower~1"),
            ("Stone5_2", "Stone5~2"),
            ("_", "~"),
        ] {
            let meta: serde_json::Value =
                serde_json::from_str(&first[&format!("src/{stem}/init.meta.json")]).unwrap();
            // Unchanged physical names do not need overrides; all of these do.
            assert_eq!(meta["name"], name);
        }
        assert_eq!(sync(&input), first);
        let output = dir.path().join("rebuilt.rbxl");
        let result = Command::new(ROJO_PATH)
            .args(["--color", "never", "build"])
            .arg(&project)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rebuilt = rbx_binary::from_reader(fs_err::File::open(&output).unwrap()).unwrap();
        assert_eq!(records(&rebuilt), records(&dom));
        assert_eq!(sync(&output), first);
    }
}

/// Reconciliation must pair released duplicate paths with their original logical
/// siblings, not regenerate them with another sibling's filesystem-only metadata.
#[test]
fn legacy_duplicate_reconciliation_preserves_metadata_and_order() {
    use rbx_dom_weak::InstanceBuilder;

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs_err::create_dir_all(project.join("src")).unwrap();
    fs_err::write(
        project.join("default.project.json"),
        r#"{
        "name":"Legacy", "syncbackRules":{"instancesAsDirectories":true},
        "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}
    }"#,
    )
    .unwrap();
    let layouts = [
        ("Twin", "Folder", "Twin/init.meta.json"),
        ("Twin~2.luau", "ModuleScript", "Twin~2.meta.jsonc"),
        ("Twin~3", "Folder", "Twin~3/init.meta.jsonc"),
        ("Twin~4", "ModuleScript", "Twin~4/init.meta.json"),
    ];
    for (index, (path, class, meta)) in layouts.iter().enumerate() {
        let number = index + 1;
        if path.ends_with(".luau") {
            fs_err::write(project.join("src").join(path), format!("return {number}")).unwrap();
        } else {
            let folder = project.join("src").join(path);
            fs_err::create_dir(&folder).unwrap();
            if *class == "ModuleScript" {
                fs_err::write(folder.join("init.luau"), format!("return {number}")).unwrap();
            } else {
                fs_err::create_dir(folder.join(format!("Marker{number}"))).unwrap();
            }
        }
        let mut metadata = serde_json::json!({
            "$schema": format!("schema-{number}"),
            "id": format!("sibling-{number}"), "ignoreUnknownInstances": index % 2 == 0,
            "name": "Twin"
        });
        if *class == "Folder" {
            metadata["className"] = serde_json::json!(class);
        }
        fs_err::write(
            project.join("src").join(meta),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }
    let output = dir.path().join("rebuilt.rbxl");
    let build = || {
        let result = Command::new(ROJO_PATH)
            .arg("build")
            .arg(&project)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        rbx_binary::from_reader(fs_err::File::open(&output).unwrap()).unwrap()
    };
    let check_order = |dom: &WeakDom| {
        let storage = dom
            .descendants()
            .find(|instance| instance.class == "ReplicatedStorage")
            .unwrap();
        let actual: Vec<_> = storage
            .children()
            .iter()
            .map(|id| dom.get_by_ref(*id).unwrap())
            .filter(|child| child.name == "Twin")
            .map(|child| {
                if let Some(Variant::String(source)) = child.properties.get(&ustr("Source")) {
                    source
                        .trim()
                        .strip_prefix("return ")
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                } else {
                    dom.get_by_ref(child.children()[0])
                        .unwrap()
                        .name
                        .strip_prefix("Marker")
                        .unwrap()
                        .parse()
                        .unwrap()
                }
            })
            .collect();
        assert_eq!(actual, [1, 2, 3, 4]);
    };
    let mut dom = build();
    check_order(&dom);
    let original = read_files(&project);
    for pass in 0..2 {
        let storage = dom
            .descendants()
            .find(|instance| instance.class == "ReplicatedStorage")
            .unwrap()
            .referent();
        if pass == 0 {
            dom.insert(
                storage,
                InstanceBuilder::new("Folder").with_name("Unrelated"),
            );
        } else {
            dom.get_by_ref_mut(storage)
                .unwrap()
                .properties
                .insert(ustr("Archivable"), Variant::Bool(false));
        }
        let input = dir.path().join("edited.rbxl");
        rbx_binary::to_writer(fs_err::File::create(&input).unwrap(), &dom, &[storage]).unwrap();
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let files = read_files(&project);
        for (_, _, meta) in &layouts {
            let path = format!("src/{meta}");
            let before = jsonc_parser::parse_to_serde_value(&original[&path], &Default::default())
                .unwrap()
                .unwrap();
            let after = jsonc_parser::parse_to_serde_value(
                files
                    .get(&path)
                    .unwrap_or_else(|| panic!("Missing legacy metadata {path}.")),
                &Default::default(),
            )
            .unwrap()
            .unwrap();
            for key in ["$schema", "id"] {
                assert_eq!(
                    after[key], before[key],
                    "Metadata {key} changed for {path}."
                );
            }
            assert_eq!(
                after["ignoreUnknownInstances"].as_bool().unwrap_or(false),
                before["ignoreUnknownInstances"].as_bool().unwrap()
            );
            if meta.contains('~') {
                assert_eq!(after["name"], "Twin");
            }
        }
        assert!(!files.keys().any(|path| path.contains(".__")));
        dom = build();
        check_order(&dom);
    }
}

/// Class replacements at an owned directory keep ignored files and filesystem-only
/// metadata, while removing stale children and writing the incoming subtree.
#[test]
fn directory_class_replacements_preserve_owned_paths() {
    use rbx_dom_weak::InstanceBuilder;

    for classes in [
        vec![("Folder", "Model")],
        vec![("Folder", "Model"), ("Configuration", "Tool")],
        vec![("Folder", "Tool"), ("Configuration", "Model")],
        vec![("Folder", "Model"), ("Configuration", "Configuration")],
        vec![("Folder", "ModuleScript")],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        fs_err::write(
            &project,
            r#"{"name":"Replacement","globIgnorePaths":["**/keep.txt"],
                "syncbackRules":{"instancesAsDirectories":true,
                "ignorePaths":["**/keep.txt"]},
                "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        let mut storage = InstanceBuilder::new("ReplicatedStorage");
        for (index, (old_class, new_class)) in classes.iter().enumerate() {
            let stem = if index == 0 { "Twin" } else { "Twin.__2" };
            let path = dir.path().join("src").join(stem);
            fs_err::create_dir_all(path.join("Stale")).unwrap();
            fs_err::write(path.join("keep.txt"), format!("ignored-{index}")).unwrap();
            fs_err::write(
                path.join("init.meta.json"),
                serde_json::to_vec(&serde_json::json!({
                    "className":old_class, "name":"Twin", "id":format!("owner-{index}"),
                    "$schema":format!("schema-{index}"), "ignoreUnknownInstances":true
                }))
                .unwrap(),
            )
            .unwrap();
            let mut child = InstanceBuilder::new(*new_class)
                .with_name("Twin")
                .with_child(
                    InstanceBuilder::new("Folder").with_name(format!("Replacement{index}")),
                );
            if *new_class == "ModuleScript" {
                child = child.with_property("Source", format!("return {index}"));
            }
            storage = storage.with_child(child);
        }
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(storage));
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        for _ in 0..2 {
            let result = Command::new(ROJO_PATH)
                .arg("syncback")
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            for (index, (_, class)) in classes.iter().enumerate() {
                let stem = if index == 0 { "Twin" } else { "Twin.__2" };
                let path = dir.path().join("src").join(stem);
                assert_eq!(
                    fs_err::read_to_string(path.join("keep.txt")).unwrap(),
                    format!("ignored-{index}")
                );
                assert!(!path.join("Stale").exists());
                let meta: serde_json::Value =
                    serde_json::from_slice(&fs_err::read(path.join("init.meta.json")).unwrap())
                        .unwrap();
                assert_eq!(meta["id"], format!("owner-{index}"));
                assert_eq!(meta["$schema"], format!("schema-{index}"));
                assert_eq!(meta["ignoreUnknownInstances"], true);
                let output = dir.path().join("rebuilt.rbxl");
                let result = Command::new(ROJO_PATH)
                    .arg("build")
                    .arg(&project)
                    .arg("--output")
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
                let storage = rebuilt
                    .descendants()
                    .find(|inst| inst.class == "ReplicatedStorage")
                    .unwrap();
                let child = rebuilt.get_by_ref(storage.children()[index]).unwrap();
                assert_eq!(child.name, "Twin");
                assert_eq!(child.class.as_str(), *class);
                assert_eq!(child.children().len(), 1);
                assert_eq!(
                    rebuilt.get_by_ref(child.children()[0]).unwrap().name,
                    format!("Replacement{index}")
                );
                if *class == "ModuleScript" {
                    assert_eq!(
                        child.properties[&ustr("Source")],
                        Variant::String(format!("return {index}"))
                    );
                }
            }
        }
    }
}

/// Reordered mixed-class duplicates must not overwrite an already retained owner.
#[test]
fn directory_class_replacement_conflicts_preserve_all_sources() {
    use rbx_dom_weak::InstanceBuilder;

    for retained_class in ["Folder", "Model", "ModuleScript"] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        fs_err::write(
            &project,
            r#"{"name":"Conflict","syncbackRules":{"instancesAsDirectories":true},
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        for (stem, class) in [("Twin", "Configuration"), ("Twin.__2", retained_class)] {
            let path = dir.path().join("src").join(stem);
            fs_err::create_dir_all(&path).unwrap();
            let mut meta = serde_json::json!({"name":"Twin", "id":stem});
            if class != "ModuleScript" {
                meta["className"] = serde_json::json!(class);
            }
            fs_err::write(
                path.join("init.meta.json"),
                serde_json::to_vec(&meta).unwrap(),
            )
            .unwrap();
            if class == "ModuleScript" {
                fs_err::write(path.join("init.luau"), "return 1").unwrap();
            }
        }
        let before = read_files(dir.path());
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage")
                    .with_child(InstanceBuilder::new(retained_class).with_name("Twin"))
                    .with_child(InstanceBuilder::new("Tool").with_name("Twin")),
            ),
        );
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("another source owns its destination"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        for (path, contents) in before {
            assert_eq!(
                fs_err::read_to_string(dir.path().join(path)).unwrap(),
                contents
            );
        }
        assert!(!dir.path().join("src.rbxm").exists());
    }
}

/// A class and model-layout change may reuse a recursively removed source path.
/// Ignored files must block that destructive relayout before any files change.
#[test]
fn directory_class_model_relayout_reuses_removed_path_safely() {
    use rbx_dom_weak::InstanceBuilder;

    for ignored in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        fs_err::write(
            &project,
            serde_json::to_vec(&serde_json::json!({
                "name":"Relayout", "syncbackRules":{"instancesAsDirectories":true,
                    "modelTrees":[{"pattern":"ReplicatedStorage/Twin"}],
                    "ignorePaths":if ignored { vec!["**/keep.txt"] } else { vec![] }},
                "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let source = dir.path().join("src/Twin.rbxm");
        fs_err::create_dir_all(source.join("Stale")).unwrap();
        fs_err::write(source.join("keep.txt"), "original ignored bytes").unwrap();
        fs_err::write(
            source.join("init.meta.json"),
            r#"{"className":"Folder","name":"Twin","id":"old-folder"}"#,
        )
        .unwrap();
        let before = read_files(dir.path());
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage").with_child(
                    InstanceBuilder::new("Model")
                        .with_name("Twin")
                        .with_child(InstanceBuilder::new("Folder").with_name("Replacement")),
                ),
            ),
        );
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            !ignored,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if ignored {
            assert!(String::from_utf8_lossy(&result.stderr).contains("ignorePaths"));
            for (path, contents) in before {
                assert_eq!(
                    fs_err::read_to_string(dir.path().join(path)).unwrap(),
                    contents
                );
            }
        } else {
            assert!(source.is_file());
            let model = rbx_binary::from_reader(fs_err::File::open(&source).unwrap()).unwrap();
            let root = model.get_by_ref(model.root().children()[0]).unwrap();
            assert_eq!(root.class.as_str(), "Model");
            assert_eq!(root.children().len(), 1);
            assert_eq!(
                model.get_by_ref(root.children()[0]).unwrap().name,
                "Replacement"
            );
            assert!(!source.join("Stale").exists());
        }
    }
}

/// Removing an init script changes the layout even though both owners use a directory.
#[test]
fn directory_class_script_relayout_removes_stale_init_before_writing() {
    use rbx_dom_weak::InstanceBuilder;

    for ignored in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        fs_err::write(
            &project,
            serde_json::to_vec(&serde_json::json!({
                "name":"ScriptRelayout", "syncbackRules":{"instancesAsDirectories":true,
                    "ignorePaths":if ignored { vec!["**/keep.txt"] } else { vec![] }},
                "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let source = dir.path().join("src/Twin");
        fs_err::create_dir_all(source.join("Stale")).unwrap();
        fs_err::write(source.join("init.luau"), "return 'old script'").unwrap();
        fs_err::write(source.join("keep.txt"), "original bytes").unwrap();
        let before = read_files(dir.path());
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage").with_child(
                    InstanceBuilder::new("Folder")
                        .with_name("Twin")
                        .with_child(InstanceBuilder::new("Folder").with_name("Replacement")),
                ),
            ),
        );
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            !ignored,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if ignored {
            assert!(String::from_utf8_lossy(&result.stderr).contains("ignorePaths"));
            for (path, contents) in before {
                assert_eq!(
                    fs_err::read_to_string(dir.path().join(path)).unwrap(),
                    contents
                );
            }
        } else {
            assert!(!source.join("init.luau").exists());
            assert!(!source.join("Stale").exists());
            assert!(!source.join("keep.txt").exists());
            assert!(source.is_dir());
            let output = dir.path().join("rebuilt.rbxl");
            let result = Command::new(ROJO_PATH)
                .arg("build")
                .arg(&project)
                .arg("--output")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
            let child = rebuilt
                .descendants()
                .find(|inst| inst.name == "Twin")
                .unwrap();
            assert_eq!(child.class.as_str(), "Folder");
            assert_eq!(child.children().len(), 1);
            assert_eq!(
                rebuilt.get_by_ref(child.children()[0]).unwrap().name,
                "Replacement"
            );
        }
    }
}

/// Exact normalized destinations justify renames without losing their metadata.
#[test]
fn directory_normalized_renames_preserve_metadata() {
    use rbx_dom_weak::InstanceBuilder;

    for (old_name, new_name, stem) in [("Twin", "twin", "Twin"), ("a?", "a*", "a_")] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        fs_err::create_dir_all(dir.path().join("src")).unwrap();
        fs_err::write(
            &project,
            r#"{"name":"Rename","syncbackRules":{"instancesAsDirectories":true},
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        fs_err::write(dir.path().join(format!("src/{stem}.luau")), "return 'old'").unwrap();
        fs_err::write(
            dir.path().join(format!("src/{stem}.meta.json")),
            serde_json::to_vec(&serde_json::json!({"name":old_name, "id":"owner",
                "$schema":"schema.json", "ignoreUnknownInstances":true}))
            .unwrap(),
        )
        .unwrap();
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage").with_child(
                    InstanceBuilder::new("ModuleScript")
                        .with_name(new_name)
                        .with_property("Source", "return 'renamed'"),
                ),
            ),
        );
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        for _ in 0..2 {
            let result = Command::new(ROJO_PATH)
                .arg("syncback")
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let meta: serde_json::Value = serde_json::from_slice(
                &fs_err::read(dir.path().join(format!("src/{stem}.meta.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(meta["id"], "owner");
            assert_eq!(meta["$schema"], "schema.json");
            assert_eq!(meta["ignoreUnknownInstances"], true);
            assert_eq!(meta["name"], new_name);
            assert_eq!(
                fs_err::read_to_string(dir.path().join(format!("src/{stem}.luau"))).unwrap(),
                "return 'renamed'"
            );
            let output = dir.path().join("rebuilt.rbxl");
            let result = Command::new(ROJO_PATH)
                .arg("build")
                .arg(&project)
                .arg("--output")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
            let child = rebuilt
                .descendants()
                .find(|child| child.class == "ModuleScript")
                .unwrap();
            assert_eq!(child.name, new_name);
            assert_eq!(
                child.properties[&ustr("Source")],
                Variant::String("return 'renamed'".into())
            );
        }
    }
}

/// Full reconciliation appends after retained sparse ordinals across all layouts.
#[test]
fn directory_full_sync_appends_after_retained_ordinals() {
    use rbx_dom_weak::InstanceBuilder;

    for (last_stem, last_directory, mixed_case) in [
        ("Twin~8", false, false),
        ("Twin.__8", false, false),
        ("Twin~8", true, false),
        ("TWIN.__8", true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        let src = dir.path().join("src");
        fs_err::create_dir_all(&src).unwrap();
        fs_err::write(
            &project,
            r#"{"name":"Append","syncbackRules":{"instancesAsDirectories":true},
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        fs_err::write(src.join("Twin.luau"), "return 1").unwrap();
        fs_err::write(
            src.join("Twin.meta.json"),
            r#"{"id":"first","$schema":"first-schema","ignoreUnknownInstances":true}"#,
        )
        .unwrap();
        let last_meta = if last_directory {
            fs_err::create_dir(src.join(last_stem)).unwrap();
            fs_err::write(src.join(last_stem).join("init.luau"), "return 8").unwrap();
            src.join(last_stem).join("init.meta.jsonc")
        } else {
            fs_err::write(src.join(format!("{last_stem}.luau")), "return 8").unwrap();
            src.join(format!("{last_stem}.meta.jsonc"))
        };
        let last_name = if mixed_case { "twin" } else { "Twin" };
        fs_err::write(
            &last_meta,
            serde_json::to_vec(&serde_json::json!({"name":last_name,
            "id":"eighth","$schema":"eighth-schema","ignoreUnknownInstances":true}))
            .unwrap(),
        )
        .unwrap();
        // Retain both suffix generations and a naturally numbered logical name.
        fs_err::write(src.join("Twin.__3.luau"), "return 3").unwrap();
        fs_err::write(
            src.join("Twin.__3.meta.json"),
            r#"{"name":"Twin","id":"third"}"#,
        )
        .unwrap();
        fs_err::create_dir(src.join("twin.__100")).unwrap();
        fs_err::write(src.join("twin.__100/init.meta.json"), r#"{"id":"natural"}"#).unwrap();
        let mut storage = InstanceBuilder::new("ReplicatedStorage");
        for number in [1, 3, 8, 10, 11] {
            let name = if number == 8 {
                last_name
            } else if mixed_case && number >= 10 {
                "TWIN"
            } else {
                "Twin"
            };
            storage = storage.with_child(
                InstanceBuilder::new("ModuleScript")
                    .with_name(name)
                    .with_property("Source", format!("return {number}")),
            );
        }
        // This pending directory owns the ninth script's sidecar destination.
        storage =
            storage.with_child(InstanceBuilder::new("Folder").with_name("Twin.__9.meta.json"));
        // A naturally numbered logical name must not raise the Twin ordinal floor.
        for _ in 0..3 {
            storage = storage.with_child(InstanceBuilder::new("Folder").with_name("twin.__100"));
        }
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(storage));
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let mut previous = None;
        for _ in 0..3 {
            let result = Command::new(ROJO_PATH)
                .arg("syncback")
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let added_name = if mixed_case { "TWIN" } else { "Twin" };
            assert_eq!(
                fs_err::read_to_string(src.join(format!("{added_name}.__10.luau"))).unwrap(),
                "return 10"
            );
            assert_eq!(
                fs_err::read_to_string(src.join(format!("{added_name}.__11.luau"))).unwrap(),
                "return 11"
            );
            assert_eq!(
                fs_err::read_to_string(src.join("Twin.__3.luau")).unwrap(),
                "return 3"
            );
            let third: serde_json::Value =
                serde_json::from_slice(&fs_err::read(src.join("Twin.__3.meta.json")).unwrap())
                    .unwrap();
            assert_eq!(third["id"], "third");
            let natural: serde_json::Value = serde_json::from_slice(
                &fs_err::read(src.join("twin.__100/init.meta.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(natural["id"], "natural");
            for ordinal in [2, 3] {
                let meta: serde_json::Value = serde_json::from_slice(
                    &fs_err::read(src.join(format!("twin.__100.__{ordinal}/init.meta.json")))
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(meta["name"], "twin.__100");
            }
            for (path, id) in [
                (src.join("Twin.meta.json"), "first"),
                (last_meta.clone(), "eighth"),
            ] {
                let meta = jsonc_parser::parse_to_serde_value(
                    &fs_err::read_to_string(path).unwrap(),
                    &Default::default(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(meta["id"], id);
                assert_eq!(meta["$schema"], format!("{id}-schema"));
                assert_eq!(meta["ignoreUnknownInstances"], true);
            }
            let files = read_files(&src);
            if let Some(previous) = previous.replace(files.clone()) {
                assert_eq!(files, previous);
            }
            let output = dir.path().join("rebuilt.rbxl");
            let result = Command::new(ROJO_PATH)
                .arg("build")
                .arg(&project)
                .arg("--output")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
            let storage = rebuilt
                .descendants()
                .find(|child| child.class == "ReplicatedStorage")
                .unwrap();
            let sources: Vec<_> = storage
                .children()
                .iter()
                .filter_map(|id| {
                    let child = rebuilt.get_by_ref(*id).unwrap();
                    child
                        .name
                        .eq_ignore_ascii_case("Twin")
                        .then(|| child.properties[&ustr("Source")].clone())
                })
                .collect();
            assert_eq!(
                sources,
                [1, 3, 8, 10, 11].map(|number| Variant::String(format!("return {number}")))
            );
            rbx_binary::to_writer(
                fs_err::File::create(&input).unwrap(),
                &rebuilt,
                rebuilt.root().children(),
            )
            .unwrap();
        }
    }
}

/// Exhausted ordinals must fail without wrapping or falling back to a model file.
#[test]
fn directory_full_sync_ordinal_exhaustion_preserves_sources() {
    use rbx_dom_weak::InstanceBuilder;

    for blocked_last in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        let src = dir.path().join("src");
        fs_err::create_dir_all(&src).unwrap();
        fs_err::write(
            &project,
            r#"{"name":"Exhaustion","syncbackRules":{"instancesAsDirectories":true},
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        let ordinal = if blocked_last { u64::MAX - 1 } else { u64::MAX };
        for (stem, number) in [("Twin".to_owned(), 1), (format!("Twin~{ordinal}"), 8)] {
            fs_err::write(src.join(format!("{stem}.luau")), format!("return {number}")).unwrap();
            fs_err::write(src.join(format!("{stem}.meta.json")), r#"{"name":"Twin"}"#).unwrap();
        }
        if blocked_last {
            fs_err::write(src.join(format!("Twin.__{}.meta.jsonc", u64::MAX)), "{}").unwrap();
        }
        let before = read_files(&src);
        let mut storage = InstanceBuilder::new("ReplicatedStorage");
        for number in [1, 8, 9] {
            storage = storage.with_child(
                InstanceBuilder::new("ModuleScript")
                    .with_name("Twin")
                    .with_property("Source", format!("return {number}")),
            );
        }
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(storage));
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("Sibling ordinals are exhausted"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(read_files(&src), before);
        assert!(!dir.path().join("src.rbxm").exists());
    }
}

/// Fresh serialization retains root identity but never stale script properties.
#[test]
fn directory_script_to_folder_relayout_preserves_root_metadata() {
    use rbx_dom_weak::InstanceBuilder;

    for meta_name in ["init.meta.json", "init.meta.jsonc"] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        let source = dir.path().join("src/Twin");
        fs_err::create_dir_all(source.join("Retained")).unwrap();
        fs_err::write(
            &project,
            r#"{"name":"Metadata","syncbackRules":{"instancesAsDirectories":true},
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .unwrap();
        fs_err::write(source.join("init.luau"), "return 'stale'").unwrap();
        fs_err::write(
            source.join(meta_name),
            r#"{"id":"owner","$schema":"schema.json","ignoreUnknownInstances":true,
            "properties":{"Archivable":false},"attributes":{"Stale":"old"}}"#,
        )
        .unwrap();
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage").with_child(
                    InstanceBuilder::new("Folder")
                        .with_name("Twin")
                        .with_child(InstanceBuilder::new("Folder").with_name("Retained"))
                        .with_child(InstanceBuilder::new("Folder").with_name("Replacement")),
                ),
            ),
        );
        let input = dir.path().join("incoming.rbxl");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &dom,
            dom.root().children(),
        )
        .unwrap();
        for _ in 0..2 {
            let result = Command::new(ROJO_PATH)
                .arg("syncback")
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let meta_path = ["init.meta.json", "init.meta.jsonc"]
                .into_iter()
                .map(|name| source.join(name))
                .find(|path| path.is_file())
                .unwrap();
            let meta: serde_json::Value =
                serde_json::from_slice(&fs_err::read(meta_path).unwrap()).unwrap();
            assert_eq!(meta["id"], "owner");
            assert_eq!(meta["$schema"], "schema.json");
            assert_eq!(meta["ignoreUnknownInstances"], true);
            assert!(meta["properties"].get("Source").is_none());
            assert!(meta["properties"].get("Archivable").is_none());
            assert!(meta["attributes"].get("Stale").is_none());
            assert!(!source.join("init.luau").exists());
            let output = dir.path().join("rebuilt.rbxl");
            let result = Command::new(ROJO_PATH)
                .arg("build")
                .arg(&project)
                .arg("--output")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
            let child = rebuilt
                .descendants()
                .find(|child| child.name == "Twin")
                .unwrap();
            assert_eq!(child.class.as_str(), "Folder");
            let names: Vec<_> = child
                .children()
                .iter()
                .map(|id| rebuilt.get_by_ref(*id).unwrap().name.as_str())
                .collect();
            assert_eq!(names, ["Retained", "Replacement"]);
            assert!(!child.properties.contains_key(&ustr("Source")));
        }
    }
}

/// Reads every file under `dir` into a map from relative path to contents. Paths
/// always use `/`, so tests can name them the same way on every platform.
fn read_files(dir: &Path) -> std::collections::BTreeMap<String, String> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let path = entry.path().strip_prefix(dir).unwrap();
            let key = path
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            (key, fs_err::read_to_string(entry.path()).unwrap())
        })
        .collect()
}

/// Every CLI JSON writer normalizes numbers without losing meaningful fractions,
/// and rebuilding or syncing again must not bring the original float tokens back.
#[test]
fn json_numeric_normalization_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs_err::create_dir_all(project.join("src/Metadata")).unwrap();
    fs_err::write(
        project.join("default.project.json"),
        r#"{
  "name": "Numbers",
  "tree": {
    "$className": "Folder",
    "src": { "$path": "src" }
  }
}"#,
    )
    .unwrap();
    for (file, contents) in [
        (
            "src/Metadata/init.meta.json",
            r#"{"className":"NumberValue"}"#,
        ),
        ("src/Model.model.json", r#"{"className":"NumberValue"}"#),
        ("src/Default.model.json", r#"{"className":"NumberValue"}"#),
        (
            "src/Project.project.json",
            r#"{"name":"Project","tree":{"$className":"NumberValue"}}"#,
        ),
    ] {
        fs_err::write(project.join(file), contents).unwrap();
    }

    let cases = [
        ("Integral", 2.0, 2.0),
        ("NearAbove", 1.0000005, 1.0),
        ("NearBelow", 0.9999995, 1.0),
        ("Negative", -1.0000005, -1.0),
        ("NegativeZero", -0.0, 0.0),
        ("ZeroAbove", 0.000001, 0.0),
        ("ZeroBelow", -0.000001, 0.0),
        ("OutsideTolerance", 1.000002, 1.000002),
        ("SmallFraction", 0.000002, 0.000002),
        ("Fraction", 0.25, 0.25),
        ("LargeFraction", 1_000_000_000.25, 1_000_000_000.25),
    ];
    let mut attributes = rbx_dom_weak::types::Attributes::new();
    for (name, input, _) in cases {
        attributes.insert(name.into(), Variant::Float64(input));
    }
    let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
    let src = place.insert(
        place.root_ref(),
        rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
    );
    for name in ["Metadata", "Model", "Project"] {
        place.insert(
            src,
            rbx_dom_weak::InstanceBuilder::new("NumberValue")
                .with_name(name)
                .with_property("Attributes", attributes.clone())
                .with_property("Value", 2.0000005_f64),
        );
    }
    place.insert(
        src,
        rbx_dom_weak::InstanceBuilder::new("NumberValue")
            .with_name("Default")
            .with_property("Value", 0.0000005_f64),
    );
    let input = dir.path().join("input.rbxm");
    rbx_binary::to_writer(
        fs_err::File::create(&input).unwrap(),
        &place,
        &[place.root_ref()],
    )
    .unwrap();
    let sync = |input: &Path| {
        let status = Command::new(ROJO_PATH)
            .args(["--color", "never", "syncback"])
            .arg(&project)
            .arg("--input")
            .arg(input)
            .arg("--non-interactive")
            .status()
            .unwrap();
        assert!(status.success());
        read_files(&project)
    };
    let first = sync(&input);
    let default: serde_json::Value =
        serde_json::from_str(&first["src/Default.model.json"]).unwrap();
    assert!(default["properties"].get("Value").is_none());
    for (file, properties_path, attributes_path) in [
        ("src/Metadata/init.meta.json", "/properties", "/attributes"),
        ("src/Model.model.json", "/properties", "/attributes"),
        (
            "src/Project.project.json",
            "/tree/$properties",
            "/tree/$attributes",
        ),
    ] {
        // Integer JSON number types distinguish `2` from `2.0` without snapshots.
        let json: serde_json::Value = serde_json::from_str(&first[file]).unwrap();
        let properties = json.pointer(properties_path).unwrap();
        assert_eq!(properties["Value"].as_i64(), Some(2), "{file}");
        let attributes = json.pointer(attributes_path).unwrap();
        for (name, _, expected) in cases {
            let number = &attributes[name];
            assert_eq!(number.as_f64(), Some(expected), "{file} {name}");
            if expected.fract() == 0.0 {
                assert_eq!(number.as_i64(), Some(expected as i64), "{file} {name}");
            }
        }
    }
    pretty_assertions::assert_eq!(sync(&input), first);

    let output = dir.path().join("rebuilt.rbxm");
    let status = Command::new(ROJO_PATH)
        .args(["--color", "never", "build"])
        .arg(&project)
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    let built = rbx_binary::from_reader(fs_err::read(&output).unwrap().as_slice()).unwrap();
    for name in ["Metadata", "Model", "Project"] {
        let instance = built
            .descendants()
            .find(|instance| instance.name == name)
            .unwrap();
        assert_eq!(
            instance.properties.get(&ustr("Value")),
            Some(&Variant::Float64(2.0))
        );
        let Some(Variant::Attributes(attributes)) = instance.properties.get(&ustr("Attributes"))
        else {
            panic!("{name} should retain its numeric attributes");
        };
        for (attribute, _, expected) in cases {
            assert_eq!(
                attributes.get(attribute),
                Some(&Variant::Float64(expected)),
                "{name} {attribute}"
            );
        }
    }
    pretty_assertions::assert_eq!(sync(&output), first);
    pretty_assertions::assert_eq!(sync(&output), first);
}

/// Script suffixes describe their class and RunContext, independently of the
/// legacy emission setting, custom legacy rules, or directory layout.
#[test]
fn script_kind_suffixes_round_trip() {
    let run_contexts = &rbx_reflection_database::get()
        .unwrap()
        .enums
        .get("RunContext")
        .unwrap()
        .items;
    let cases = [
        ("Module", "ModuleScript", None, "luau"),
        ("Local", "LocalScript", None, "local.luau"),
        ("Legacy", "Script", Some("Legacy"), "legacy.luau"),
        ("Server", "Script", Some("Server"), "server.luau"),
        ("Client", "Script", Some("Client"), "client.luau"),
        ("Plugin", "Script", Some("Plugin"), "plugin.luau"),
    ];
    for emit_legacy in [None, Some(false), Some(true)] {
        for directories in [false, true] {
            for custom_rules in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let project = dir.path().join("project");
                fs_err::create_dir_all(project.join("src")).unwrap();
                let mut config = serde_json::json!({
                    "name": "ScriptKinds",
                    "syncbackRules": { "instancesAsDirectories": directories },
                    "tree": {
                        "$className": "Folder",
                        "src": { "$path": "src" }
                    }
                });
                if let Some(emit_legacy) = emit_legacy {
                    config["emitLegacyScripts"] = emit_legacy.into();
                }
                if custom_rules {
                    config["syncRules"] = serde_json::json!([
                        {
                            "pattern": "*.legacy.luau",
                            "use": "legacyServerScript",
                            "suffix": ".legacy.luau"
                        },
                        {
                            "pattern": "*.local.luau",
                            "use": "legacyClientScript",
                            "suffix": ".local.luau"
                        }
                    ]);
                }
                fs_err::write(
                    project.join("default.project.json"),
                    serde_json::to_vec_pretty(&config).unwrap(),
                )
                .unwrap();
                let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
                let src = place.insert(
                    place.root_ref(),
                    rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
                );
                for (name, class, context, _) in cases {
                    for children in [false, true] {
                        let name = if children {
                            format!("{name}WithChildren")
                        } else {
                            name.to_owned()
                        };
                        let mut builder = rbx_dom_weak::InstanceBuilder::new(class)
                            .with_name(name)
                            .with_property("Source", "return 42");
                        if let Some(context) = context {
                            builder = builder.with_property(
                                "RunContext",
                                rbx_dom_weak::types::Enum::from_u32(run_contexts[context]),
                            );
                        }
                        let script = place.insert(src, builder);
                        if children {
                            place.insert(
                                script,
                                rbx_dom_weak::InstanceBuilder::new("ModuleScript")
                                    .with_name("Child")
                                    .with_property("Source", "return 7"),
                            );
                        }
                    }
                }
                let input = dir.path().join("input.rbxm");
                rbx_binary::to_writer(
                    fs_err::File::create(&input).unwrap(),
                    &place,
                    &[place.root_ref()],
                )
                .unwrap();
                let sync = |input: &Path| {
                    let output = Command::new(ROJO_PATH)
                        .args(["--color", "never", "syncback"])
                        .arg(&project)
                        .arg("--input")
                        .arg(input)
                        .arg("--non-interactive")
                        .output()
                        .unwrap();
                    assert!(output.status.success(), "{output:?}");
                    read_files(&project)
                };
                let first = sync(&input);
                for (name, _, _, extension) in cases {
                    for children in [false, true] {
                        let file = if children {
                            format!("src/{name}WithChildren/init.{extension}")
                        } else {
                            format!("src/{name}.{extension}")
                        };
                        assert_eq!(
                            first.get(&file).map(String::as_str),
                            Some("return 42"),
                            "Missing {file} with emitLegacyScripts {emit_legacy:?}, \
                             instancesAsDirectories {directories}, custom rules {custom_rules}."
                        );
                    }
                }
                pretty_assertions::assert_eq!(sync(&input), first);
                let output_path = dir.path().join("rebuilt.rbxm");
                let output = Command::new(ROJO_PATH)
                    .args(["--color", "never", "build"])
                    .arg(&project)
                    .arg("--output")
                    .arg(&output_path)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                let built = rbx_binary::from_reader(fs_err::read(&output_path).unwrap().as_slice())
                    .unwrap();
                for (name, class, context, _) in cases {
                    for children in [false, true] {
                        let name = if children {
                            format!("{name}WithChildren")
                        } else {
                            name.to_owned()
                        };
                        let instance = built
                            .descendants()
                            .find(|instance| instance.name == name)
                            .unwrap();
                        assert_eq!(instance.class.as_str(), class, "{name}");
                        assert_eq!(
                            instance.properties.get(&ustr("Source")),
                            Some(&Variant::String("return 42".into())),
                            "{name}"
                        );
                        if let Some(context) = context {
                            let actual = instance
                                .properties
                                .get(&ustr("RunContext"))
                                .cloned()
                                .unwrap_or_else(|| {
                                    rbx_dom_weak::types::Enum::from_u32(run_contexts["Legacy"])
                                        .into()
                                });
                            assert_eq!(
                                actual,
                                Variant::Enum(rbx_dom_weak::types::Enum::from_u32(
                                    run_contexts[context]
                                )),
                                "{name}"
                            );
                        }
                        assert_eq!(instance.children().len(), usize::from(children), "{name}");
                        if children {
                            let child = built.get_by_ref(instance.children()[0]).unwrap();
                            assert_eq!(child.name, "Child");
                            assert_eq!(child.class.as_str(), "ModuleScript");
                            assert_eq!(
                                child.properties.get(&ustr("Source")),
                                Some(&Variant::String("return 7".into()))
                            );
                        }
                    }
                }
                pretty_assertions::assert_eq!(sync(&output_path), first);

                let scripts = place
                    .descendants()
                    .filter(|instance| instance.properties.contains_key(&ustr("Source")))
                    .map(|instance| instance.referent())
                    .collect::<Vec<_>>();
                for script in scripts {
                    place
                        .get_by_ref_mut(script)
                        .unwrap()
                        .properties
                        .insert(ustr("Source"), Variant::String("return 84".into()));
                }
                rbx_binary::to_writer(
                    fs_err::File::create(&input).unwrap(),
                    &place,
                    &[place.root_ref()],
                )
                .unwrap();
                let updated = sync(&input);
                assert_eq!(
                    updated.keys().collect::<Vec<_>>(),
                    first.keys().collect::<Vec<_>>()
                );
                for (file, contents) in &updated {
                    if file.ends_with(".luau") {
                        assert_eq!(contents, "return 84", "{file}");
                    }
                }
                pretty_assertions::assert_eq!(sync(&output_path), first);
            }
        }
    }
}

/// Existing legacy-emission files must not keep ambiguous suffixes after their
/// script sources change, or the next build could produce a different class.
#[test]
fn existing_scripts_adopt_canonical_suffixes() {
    for directories in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs_err::create_dir_all(project.join("src")).unwrap();
        fs_err::write(
            project.join("default.project.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "ExistingScriptKinds",
                "emitLegacyScripts": true,
                "syncbackRules": { "instancesAsDirectories": directories },
                "tree": { "$className": "Folder", "src": { "$path": "src" } }
            }))
            .unwrap(),
        )
        .unwrap();
        for file in ["VFX.client.luau", "Boot.server.luau"] {
            fs_err::write(project.join("src").join(file), "return 1").unwrap();
        }
        let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
        let src = place.insert(
            place.root_ref(),
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
        );
        for (name, class) in [("VFX", "LocalScript"), ("Boot", "Script")] {
            place.insert(
                src,
                rbx_dom_weak::InstanceBuilder::new(class)
                    .with_name(name)
                    .with_property("Source", "return 2"),
            );
        }
        let input = dir.path().join("input.rbxm");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &place,
            &[place.root_ref()],
        )
        .unwrap();
        let sync = || {
            let output = Command::new(ROJO_PATH)
                .args(["--color", "never", "syncback"])
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            read_files(&project)
        };
        let first = sync();
        assert_eq!(
            first.get("src/VFX.local.luau").map(String::as_str),
            Some("return 2")
        );
        assert_eq!(
            first.get("src/Boot.legacy.luau").map(String::as_str),
            Some("return 2")
        );
        assert!(!first.contains_key("src/VFX.client.luau"));
        assert!(!first.contains_key("src/Boot.server.luau"));
        pretty_assertions::assert_eq!(sync(), first);
    }
}

/// Canonicalizing two same-named LocalScripts must allocate distinct paths
/// rather than overwriting the script already using the destination suffix.
#[test]
fn canonical_script_suffixes_preserve_duplicate_sources() {
    for changed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs_err::create_dir_all(project.join("src")).unwrap();
        fs_err::write(
            project.join("default.project.json"),
            r#"{
  "name": "DuplicateScripts",
  "syncRules": [
    { "pattern": "*.client.luau", "use": "legacyClientScript", "suffix": ".client.luau" }
  ],
  "syncbackRules": { "instancesAsDirectories": true },
  "tree": { "$className": "Folder", "src": { "$path": "src" } }
}"#,
        )
        .unwrap();
        let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
        let src = place.insert(
            place.root_ref(),
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
        );
        for (file, source) in [
            ("Effect.client.luau", "return 1"),
            ("Effect.local.luau", "return 2"),
        ] {
            fs_err::write(project.join("src").join(file), source).unwrap();
            place.insert(
                src,
                rbx_dom_weak::InstanceBuilder::new("LocalScript")
                    .with_name("Effect")
                    .with_property(
                        "Source",
                        if changed {
                            format!("{source} -- changed")
                        } else {
                            source.to_owned()
                        },
                    ),
            );
        }
        let input = dir.path().join("input.rbxm");
        rbx_binary::to_writer(
            fs_err::File::create(&input).unwrap(),
            &place,
            &[place.root_ref()],
        )
        .unwrap();
        let sync = || {
            let output = Command::new(ROJO_PATH)
                .args(["--color", "never", "syncback"])
                .arg(&project)
                .arg("--input")
                .arg(&input)
                .arg("--non-interactive")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            read_files(&project)
        };
        let first = sync();
        let mut sources = first
            .iter()
            .filter(|(file, _)| file.ends_with(".luau"))
            .map(|(file, source)| {
                assert!(file.ends_with(".local.luau"), "{file}");
                source.as_str()
            })
            .collect::<Vec<_>>();
        sources.sort();
        assert_eq!(
            sources,
            if changed {
                vec!["return 1 -- changed", "return 2 -- changed"]
            } else {
                vec!["return 1", "return 2"]
            }
        );
        pretty_assertions::assert_eq!(sync(), first);
    }
}

#[test]
fn canonical_script_suffixes_do_not_overwrite_ignored_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs_err::create_dir_all(project.join("src")).unwrap();
    fs_err::write(
        project.join("default.project.json"),
        r#"{
  "name": "UnmanagedDestination",
  "syncRules": [
    { "pattern": "*.client.luau", "use": "legacyClientScript", "suffix": ".client.luau" },
    { "pattern": "*.local.luau", "use": "ignore" }
  ],
  "syncbackRules": { "instancesAsDirectories": true },
  "tree": { "$className": "Folder", "src": { "$path": "src" } }
}"#,
    )
    .unwrap();
    fs_err::write(project.join("src/Effect.client.luau"), "return 1").unwrap();
    fs_err::write(project.join("src/Effect.local.luau"), "unmanaged contents").unwrap();
    let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
    let src = place.insert(
        place.root_ref(),
        rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
    );
    place.insert(
        src,
        rbx_dom_weak::InstanceBuilder::new("LocalScript")
            .with_name("Effect")
            .with_property("Source", "return 2"),
    );
    let input = dir.path().join("input.rbxm");
    rbx_binary::to_writer(
        fs_err::File::create(&input).unwrap(),
        &place,
        &[place.root_ref()],
    )
    .unwrap();
    let before = read_files(&project);
    let output = Command::new(ROJO_PATH)
        .args(["--color", "never", "syncback"])
        .arg(&project)
        .arg("--input")
        .arg(&input)
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unmanaged file"),
        "{output:?}"
    );
    pretty_assertions::assert_eq!(read_files(&project), before);
}

#[test]
fn replacing_reference_attribute_ids_is_stable_on_the_first_export() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs_err::create_dir_all(project.join("src/Target")).unwrap();
    fs_err::write(
        project.join("default.project.json"),
        r#"{
  "name": "ReferenceIdentity",
  "syncbackRules": { "instancesAsDirectories": true },
  "tree": { "$className": "Folder", "src": { "$path": "src" } }
}"#,
    )
    .unwrap();
    fs_err::write(project.join("src/Module.luau"), "return {}").unwrap();
    fs_err::write(
        project.join("src/Target/init.meta.json"),
        r#"{"className":"Part","attributes":{"Rojo_Id":"old-target"}}"#,
    )
    .unwrap();
    let mut dom = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
    let src = dom.insert(
        dom.root_ref(),
        rbx_dom_weak::InstanceBuilder::new("Folder").with_name("src"),
    );
    dom.insert(
        src,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Module")
            .with_property("Source", "return {}"),
    );
    let mut attributes = rbx_dom_weak::types::Attributes::new();
    attributes.insert("Rojo_Id".into(), Variant::String("new-target".into()));
    let target = dom.insert(
        src,
        rbx_dom_weak::InstanceBuilder::new("Part")
            .with_name("Target")
            .with_property("Attributes", attributes),
    );
    dom.insert(
        src,
        rbx_dom_weak::InstanceBuilder::new("ObjectValue")
            .with_name("Pointer")
            .with_property("Value", Variant::Ref(target)),
    );
    let input = temp.path().join("input.rbxm");
    rbx_binary::to_writer(
        fs_err::File::create(&input).unwrap(),
        &dom,
        &[dom.root_ref()],
    )
    .unwrap();
    let sync = || {
        let output = Command::new(ROJO_PATH)
            .args(["--color", "never", "syncback"])
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        read_files(&project)
    };
    let first = sync();
    let metadata: serde_json::Value =
        serde_json::from_str(&first["src/Target/init.meta.json"]).unwrap();
    assert!(metadata.get("id").is_none());
    assert_eq!(metadata["attributes"]["Rojo_Id"], "new-target");
    pretty_assertions::assert_eq!(sync(), first);
}

#[test]
fn script_suffix_migration_review_regressions() {
    use rbx_dom_weak::{types::Enum, InstanceBuilder};

    let contexts = &rbx_reflection_database::get().unwrap().enums["RunContext"].items;
    for directories in [false, true] {
        for extension in ["lua", "luau"] {
            for case in [
                "initializer",
                "ignore_old",
                "ignore_new",
                "ignore_leaf_old",
                "ignore_leaf_new",
                "custom",
                "nested",
                "staged_collision",
                "adjacent",
                "fixed",
                "ignored_context",
                "ignored_leaf_context",
                "ignored_init_context",
                "ignored_leaf_unchanged",
                "ignored_init_unchanged",
                "rule_ignored",
                "rule_wrong_class",
                "rule_broad",
                "nested_new_rule_ignored",
                "nested_new_rule_wrong_class",
                "nested_new_rule_broad",
                "nested_new_rule_valid",
            ] {
                let temp = tempfile::tempdir().unwrap();
                let project = temp.path().join("project");
                fs_err::create_dir_all(project.join("src/Boot")).unwrap();
                let mut config = serde_json::json!({
                    "name": "Migration",
                    "syncbackRules": { "instancesAsDirectories": directories },
                    "tree": { "$className": "Folder", "src": { "$path": "src" } }
                });
                let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
                let root = dom.root_ref();
                let parent = if matches!(case, "fixed" | "ignored_context") {
                    root
                } else {
                    dom.insert(root, InstanceBuilder::new("Folder").with_name("src"))
                };
                let mut expected = vec![("Boot", "Script", "return 2", Some("Legacy"))];
                match case {
                    "initializer" | "ignore_old" | "ignore_new" => {
                        fs_err::write(
                            project.join(format!("src/Boot/init.server.{extension}")),
                            "return 1",
                        )
                        .unwrap();
                        dom.insert(
                            parent,
                            InstanceBuilder::new("Script")
                                .with_name("Boot")
                                .with_property("Source", "return 2"),
                        );
                        if case.starts_with("ignore") {
                            let kind = if case == "ignore_old" {
                                "server"
                            } else {
                                "legacy"
                            };
                            config["syncbackRules"]["ignorePaths"] =
                                serde_json::json!([format!("src/Boot/init.{kind}.{extension}")]);
                        }
                    }
                    "ignore_leaf_old" | "ignore_leaf_new" => {
                        fs_err::remove_dir(project.join("src/Boot")).unwrap();
                        fs_err::write(
                            project.join(format!("src/Boot.server.{extension}")),
                            "return 1",
                        )
                        .unwrap();
                        dom.insert(
                            parent,
                            InstanceBuilder::new("Script")
                                .with_name("Boot")
                                .with_property("Source", "return 2"),
                        );
                        let kind = if case == "ignore_leaf_old" {
                            "server"
                        } else {
                            "legacy"
                        };
                        config["syncbackRules"]["ignorePaths"] =
                            serde_json::json!([format!("src/Boot.{kind}.{extension}")]);
                    }
                    "custom" | "nested" => {
                        let rules = serde_json::json!([{
                            "pattern": format!("*.server.{extension}"),
                            "use": "moduleScript", "suffix": format!(".{extension}")
                        }]);
                        let (source_dir, parent) = if case == "nested" {
                            fs_err::create_dir_all(project.join("src/Nested/Content")).unwrap();
                            fs_err::write(project.join("src/Nested/default.project.json"),
                                serde_json::to_vec_pretty(&serde_json::json!({
                                    "name":"Nested", "tree":{"$className":"Folder","$path":"Content"},
                                    "syncRules": rules
                                })).unwrap()).unwrap();
                            (
                                "src/Nested/Content",
                                dom.insert(
                                    parent,
                                    InstanceBuilder::new("Folder").with_name("Nested"),
                                ),
                            )
                        } else {
                            config["syncRules"] = rules;
                            ("src", parent)
                        };
                        expected = vec![
                            ("Foo.server", "ModuleScript", "return 3", None),
                            ("Foo", "ModuleScript", "return 4", None),
                        ];
                        for (name, _, source, _) in &expected {
                            fs_err::write(
                                project.join(format!("{source_dir}/{name}.{extension}")),
                                "return 1",
                            )
                            .unwrap();
                            dom.insert(
                                parent,
                                InstanceBuilder::new("ModuleScript")
                                    .with_name(*name)
                                    .with_property("Source", *source),
                            );
                        }
                    }
                    "staged_collision" => {
                        fs_err::write(
                            project.join(format!("src/Effect.client.{extension}")),
                            "return 1",
                        )
                        .unwrap();
                        fs_err::write(project.join("src/Effect.meta.json"), r#"{"name":"Actual"}"#)
                            .unwrap();
                        config["syncRules"] = serde_json::json!([{
                            "pattern":format!("*.client.{extension}"),
                            "use":"legacyClientScript", "suffix":format!(".client.{extension}")
                        }]);
                        for (name, source) in [("Actual", "return 2"), ("Effect", "return 3")] {
                            dom.insert(
                                parent,
                                InstanceBuilder::new("LocalScript")
                                    .with_name(name)
                                    .with_property("Source", source),
                            );
                        }
                    }
                    "adjacent" => {
                        fs_err::write(
                            project.join(format!("src/Boot.server.{extension}")),
                            "return 1",
                        )
                        .unwrap();
                        fs_err::write(project.join("src/Boot.meta.json"),
                            r#"{"name":"Actual","properties":{"RunContext":{"Enum":0},"Disabled":false}}"#).unwrap();
                        expected = vec![("Actual", "Script", "return 2", Some("Client"))];
                        dom.insert(
                            parent,
                            InstanceBuilder::new("Script")
                                .with_name("Actual")
                                .with_property("Source", "return 2")
                                .with_property("Disabled", true)
                                .with_property("RunContext", Enum::from_u32(contexts["Client"])),
                        );
                    }
                    "fixed" | "ignored_context" => {
                        let path = format!("src/Boot.server.{extension}");
                        fs_err::write(project.join(&path), "return 1").unwrap();
                        config["tree"] = serde_json::json!({
                            "$className": "Folder", "Boot": { "$path": path }
                        });
                        if case == "ignored_context" {
                            config["tree"]["Boot"]["$properties"] =
                                serde_json::json!({"RunContext":{"Enum":contexts["Client"]}});
                            config["syncbackRules"]["ignoreProperties"] =
                                serde_json::json!({"LuaSourceContainer":["RunContext"]});
                            expected = vec![("Boot", "Script", "return 2", Some("Client"))];
                        }
                        dom.insert(
                            parent,
                            InstanceBuilder::new("Script")
                                .with_name("Boot")
                                .with_property("Source", "return 2"),
                        );
                    }
                    "ignored_leaf_context"
                    | "ignored_init_context"
                    | "ignored_leaf_unchanged"
                    | "ignored_init_unchanged" => {
                        let initializer = case.starts_with("ignored_init");
                        let source = if case.ends_with("unchanged") {
                            "return 1"
                        } else {
                            "return 2"
                        };
                        let path = if initializer {
                            format!("src/Boot/init.server.{extension}")
                        } else {
                            fs_err::remove_dir(project.join("src/Boot")).unwrap();
                            format!("src/Boot.server.{extension}")
                        };
                        fs_err::write(project.join(path), "return 1").unwrap();
                        config["syncbackRules"]["ignoreProperties"] =
                            serde_json::json!({"LuaSourceContainer":["RunContext"]});
                        expected = vec![("Boot", "Script", source, Some("Server"))];
                        dom.insert(
                            parent,
                            InstanceBuilder::new("Script")
                                .with_name("Boot")
                                .with_property("Source", source)
                                .with_property("RunContext", Enum::from_u32(contexts["Client"])),
                        );
                    }
                    "nested_new_rule_ignored"
                    | "nested_new_rule_wrong_class"
                    | "nested_new_rule_broad"
                    | "nested_new_rule_valid" => {
                        fs_err::remove_dir(project.join("src/Boot")).unwrap();
                        fs_err::create_dir_all(project.join("src/Nested/Content")).unwrap();
                        let broad = case.ends_with("broad");
                        let rule = serde_json::json!({
                            "pattern": if broad { "*.luau" } else { "*.local.luau" },
                            "use": if case.ends_with("ignored") { "ignore" } else if case.ends_with("wrong_class") { "moduleScript" } else { "legacyClientScript" },
                            "suffix": if broad { ".luau" } else { ".local.luau" }
                        });
                        fs_err::write(
                            project.join("src/Nested/default.project.json"),
                            serde_json::to_vec_pretty(&serde_json::json!({
                                "name":"Nested", "tree":{"$className":"Folder","$path":"Content"},
                                "syncRules":[rule]
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                        if case.ends_with("valid") {
                            config["syncRules"] = serde_json::json!([{
                                "pattern":"*.local.luau", "use":"ignore"
                            }]);
                        }
                        let nested =
                            dom.insert(parent, InstanceBuilder::new("Folder").with_name("Nested"));
                        let fresh =
                            dom.insert(nested, InstanceBuilder::new("Folder").with_name("Fresh"));
                        dom.insert(
                            fresh,
                            InstanceBuilder::new("LocalScript")
                                .with_name("Effect")
                                .with_property("Source", "return 2"),
                        );
                        expected = vec![("Effect", "LocalScript", "return 2", None)];
                    }
                    "rule_ignored" | "rule_wrong_class" | "rule_broad" => {
                        let suffix = if case == "rule_broad" { "" } else { ".client" };
                        fs_err::write(
                            project.join(format!("src/Effect{suffix}.{extension}")),
                            "return 1",
                        )
                        .unwrap();
                        config["syncRules"] = if case == "rule_broad" {
                            serde_json::json!([{
                                "pattern":format!("*.{extension}"),
                                "use":"legacyClientScript", "suffix":format!(".{extension}")
                            }])
                        } else {
                            serde_json::json!([
                                {"pattern":format!("*.client.{extension}"), "use":"legacyClientScript", "suffix":format!(".client.{extension}")},
                                {"pattern":format!("*.local.{extension}"), "use":if case == "rule_ignored" { "ignore" } else { "moduleScript" }, "suffix":format!(".local.{extension}")}
                            ])
                        };
                        dom.insert(
                            parent,
                            InstanceBuilder::new("LocalScript")
                                .with_name("Effect")
                                .with_property("Source", "return 2"),
                        );
                    }
                    _ => unreachable!(),
                }
                // The unused directory must not mask an adjacent script of the same name.
                if matches!(
                    case,
                    "custom"
                        | "nested"
                        | "staged_collision"
                        | "adjacent"
                        | "fixed"
                        | "ignored_context"
                        | "rule_ignored"
                        | "rule_wrong_class"
                        | "rule_broad"
                ) {
                    fs_err::remove_dir(project.join("src/Boot")).unwrap();
                }
                let project_path = project.join("default.project.json");
                fs_err::write(&project_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
                let input = temp.path().join("input.rbxm");
                rbx_binary::to_writer(fs_err::File::create(&input).unwrap(), &dom, &[root])
                    .unwrap();
                let before = read_files(&project);
                let sync = || {
                    Command::new(ROJO_PATH)
                        .args(["--color", "never", "syncback"])
                        .arg(&project_path)
                        .arg("--input")
                        .arg(&input)
                        .arg("--non-interactive")
                        .output()
                        .unwrap()
                };
                let output = sync();
                if case.starts_with("ignore_")
                    || case.starts_with("rule_")
                    || (case.starts_with("nested_new_rule_") && !case.ends_with("valid"))
                    || case == "staged_collision"
                {
                    assert!(!output.status.success(), "{case} {output:?}");
                    let message = if case == "staged_collision" {
                        "same destination"
                    } else if case.starts_with("rule_") || case.starts_with("nested_new_rule_") {
                        "incompatible sync rule"
                    } else {
                        "ignore pattern"
                    };
                    assert!(
                        String::from_utf8_lossy(&output.stderr).contains(message),
                        "{output:?}"
                    );
                    assert_eq!(read_files(&project), before);
                    continue;
                }
                assert!(
                    output.status.success(),
                    "{case} {directories} {extension} {output:?}"
                );
                let first = read_files(&project);
                match case {
                    "ignored_leaf_context" | "ignored_leaf_unchanged" => {
                        assert!(first.contains_key(&format!("src/Boot.server.{extension}")));
                        assert!(!first.contains_key(&format!("src/Boot.legacy.{extension}")));
                    }
                    "ignored_init_context" | "ignored_init_unchanged" => {
                        assert!(first.contains_key(&format!("src/Boot/init.server.{extension}")));
                        assert!(!first.contains_key(&format!("src/Boot/init.legacy.{extension}")));
                    }
                    "initializer" => {
                        assert!(first.contains_key(&format!("src/Boot/init.legacy.{extension}")));
                        assert!(!first.contains_key(&format!("src/Boot/init.server.{extension}")));
                    }
                    "custom" | "nested" => {
                        let prefix = if case == "nested" {
                            "src/Nested/Content"
                        } else {
                            "src"
                        };
                        assert_eq!(
                            first[&format!("{prefix}/Foo.server.{extension}")],
                            "return 3"
                        );
                        assert_eq!(first[&format!("{prefix}/Foo.{extension}")], "return 4");
                    }
                    "adjacent" if !directories => {
                        let metadata: serde_json::Value =
                            serde_json::from_str(&first["src/Boot.meta.json"]).unwrap();
                        assert_eq!(metadata["name"], "Actual");
                        assert_eq!(metadata["properties"]["Disabled"], true);
                    }
                    _ => {}
                }
                let rebuilt_path = temp.path().join("rebuilt.rbxm");
                let output = Command::new(ROJO_PATH)
                    .arg("build")
                    .arg(&project_path)
                    .arg("--output")
                    .arg(&rebuilt_path)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{case} {output:?}");
                let rebuilt =
                    rbx_binary::from_reader(fs_err::File::open(rebuilt_path).unwrap()).unwrap();
                for (name, class, source, context) in expected {
                    let actual = rebuilt
                        .descendants()
                        .find(|instance| instance.name == name)
                        .unwrap();
                    assert_eq!(actual.class.as_str(), class, "{case}");
                    assert_eq!(
                        actual.properties.get(&ustr("Source")),
                        Some(&Variant::String(source.into())),
                        "{case}"
                    );
                    if let Some(context) = context {
                        let value = actual
                            .properties
                            .get(&ustr("RunContext"))
                            .cloned()
                            .unwrap_or_else(|| Enum::from_u32(contexts["Legacy"]).into());
                        assert_eq!(
                            value,
                            Variant::Enum(Enum::from_u32(contexts[context])),
                            "{case}"
                        );
                    }
                }
                assert!(sync().status.success());
                assert_eq!(read_files(&project), first, "{case}");
            }
        }
    }
}

/// Opt-in validation against private place data, always using an isolated copy.
#[test]
#[ignore = "Requires ROJO_REAL_PROJECT, ROJO_REAL_PLACE, and ROJO_REAL_SCRIPT"]
fn real_place_script_round_trip() {
    use std::collections::{hash_map::DefaultHasher, BTreeMap};
    use std::hash::{Hash, Hasher};

    fn digest(path: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        fs_err::read(path).unwrap().hash(&mut hasher);
        hasher.finish()
    }
    fn manifest(root: &Path) -> BTreeMap<String, u64> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    digest(entry.path()),
                )
            })
            .collect()
    }
    fn binary_rotation_changes_only(
        before: &serde_json::Value,
        after: &serde_json::Value,
        rotation: bool,
    ) -> bool {
        use rbx_dom_weak::types::{Matrix3, Vector3};

        if before == after {
            return true;
        }
        if rotation {
            let flatten = |value: &serde_json::Value| -> Option<Vec<f32>> {
                value
                    .as_array()?
                    .iter()
                    .try_fold(Vec::new(), |mut values, item| {
                        if let Some(row) = item.as_array() {
                            for number in row {
                                values.push(number.as_f64()? as f32);
                            }
                        } else {
                            values.push(item.as_f64()? as f32);
                        }
                        Some(values)
                    })
            };
            if let (Some(before), Some(after)) = (flatten(before), flatten(after)) {
                let offset = match before.len() {
                    9 => 0,
                    12 => 3,
                    _ => return false,
                };
                if before.len() != after.len() || before[..offset] != after[..offset] {
                    return false;
                }
                let matrix = |values: &[f32]| {
                    Matrix3::new(
                        Vector3::new(values[0], values[1], values[2]),
                        Vector3::new(values[3], values[4], values[5]),
                        Vector3::new(values[6], values[7], values[8]),
                    )
                };
                let original = matrix(&before[offset..]);
                return original.to_basic_rotation_id().is_some_and(|id| {
                    Matrix3::from_basic_rotation_id(id).unwrap() == matrix(&after[offset..])
                });
            }
        }
        match (before, after) {
            (serde_json::Value::Object(before), serde_json::Value::Object(after)) => {
                before.len() == after.len()
                    && before.iter().all(|(key, value)| {
                        after.get(key).is_some_and(|other| {
                            binary_rotation_changes_only(
                                value,
                                other,
                                key == "orientation" || key.ends_with("CFrame"),
                            )
                        })
                    })
            }
            (serde_json::Value::Array(before), serde_json::Value::Array(after)) => {
                before.len() == after.len()
                    && before
                        .iter()
                        .zip(after)
                        .all(|(value, other)| binary_rotation_changes_only(value, other, false))
            }
            _ => false,
        }
    }
    fn scripts(
        dom: &WeakDom,
        ignored: &[String],
    ) -> BTreeMap<String, Vec<(String, Option<u32>, String)>> {
        let legacy = rbx_reflection_database::get().unwrap().enums["RunContext"].items["Legacy"];
        let mut result = BTreeMap::<String, Vec<(String, Option<u32>, String)>>::new();
        for instance in dom.descendants() {
            if !matches!(
                instance.class.as_str(),
                "Script" | "LocalScript" | "ModuleScript"
            ) {
                continue;
            }
            let mut components = vec![instance.name.clone()];
            let mut parent = instance.parent();
            while parent != dom.root_ref() && parent.is_some() {
                let ancestor = dom.get_by_ref(parent).unwrap();
                components.push(ancestor.name.clone());
                parent = ancestor.parent();
            }
            components.reverse();
            let path = components.join("/");
            if ignored
                .iter()
                .any(|ignored| path == *ignored || path.starts_with(&format!("{ignored}/")))
            {
                continue;
            }
            let context = if instance.class.as_str() == "Script" {
                Some(match instance.properties.get(&ustr("RunContext")) {
                    Some(Variant::Enum(value)) => value.to_u32(),
                    _ => legacy,
                })
            } else {
                None
            };
            let source = match instance.properties.get(&ustr("Source")) {
                Some(Variant::String(value)) => value.replace("\r\n", "\n"),
                _ => String::new(),
            };
            result
                .entry(path)
                .or_default()
                .push((instance.class.to_string(), context, source));
        }
        for values in result.values_mut() {
            values.sort();
        }
        result
    }

    let original_project = std::path::PathBuf::from(
        std::env::var_os("ROJO_REAL_PROJECT").expect("ROJO_REAL_PROJECT is required"),
    );
    let original_place = std::path::PathBuf::from(
        std::env::var_os("ROJO_REAL_PLACE").expect("ROJO_REAL_PLACE is required"),
    );
    let focus = std::env::var("ROJO_REAL_SCRIPT").expect("ROJO_REAL_SCRIPT is required");
    let project_bytes = fs_err::read(&original_project).unwrap();
    let config: serde_json::Value = serde_json::from_slice(&project_bytes).unwrap();
    let source_relative = Path::new(
        config
            .pointer("/tree/$path")
            .and_then(|value| value.as_str())
            .expect("The real project's root must have a source path"),
    );
    assert!(
        source_relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "The source path must remain inside the copied project"
    );
    let original_source = original_project.parent().unwrap().join(source_relative);
    let before_source = manifest(&original_source);
    let before_place = digest(&original_place);
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs_err::create_dir_all(&project).unwrap();
    let copied_project = project.join(original_project.file_name().unwrap());
    fs_err::write(&copied_project, &project_bytes).unwrap();
    let source = project.join(source_relative);
    for entry in walkdir::WalkDir::new(&original_source) {
        let entry = entry.unwrap();
        assert!(
            !entry.file_type().is_symlink(),
            "Refusing to test through a source symlink"
        );
        let destination = source.join(entry.path().strip_prefix(&original_source).unwrap());
        if entry.file_type().is_dir() {
            fs_err::create_dir_all(&destination).unwrap();
        } else {
            fs_err::copy(entry.path(), destination).unwrap();
        }
    }
    let input = temp.path().join("input.rbxl");
    fs_err::copy(&original_place, &input).unwrap();
    let input_dom = rbx_binary::from_reader(fs_err::File::open(&input).unwrap()).unwrap();
    let ignored = config
        .pointer("/syncbackRules/ignoreTrees")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let expected = scripts(&input_dom, &ignored);
    assert!(!expected.is_empty());
    let focus_path = expected
        .keys()
        .find(|path| path.rsplit('/').next() == Some(focus.as_str()))
        .expect("The focus script must exist in the real place")
        .clone();
    let sync = |input: &Path| {
        let output = Command::new(ROJO_PATH)
            .args(["--color", "never", "syncback"])
            .arg(&copied_project)
            .arg("--input")
            .arg(input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Syncback failed. {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (manifest(&source), fs_err::read(&copied_project).unwrap())
    };
    println!(
        "Loaded {} scripts across {} paths.",
        expected.values().map(Vec::len).sum::<usize>(),
        expected.len()
    );
    let first = sync(&input);
    println!("Completed the first export with {} files.", first.0.len());
    let second = sync(&input);
    if second != first {
        let mut paths = first
            .0
            .keys()
            .chain(second.0.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        let changed = paths
            .into_iter()
            .filter(|path| first.0.get(path) != second.0.get(path))
            .collect::<Vec<_>>();
        println!(
            "Preserved the isolated diagnostic copy at {}",
            temp.keep().display()
        );
        panic!(
            "Repeated syncback changed {} files. First changes {:?}. Project changed {}.",
            changed.len(),
            changed.iter().take(20).collect::<Vec<_>>(),
            second.1 != first.1
        );
    }
    println!("Verified the repeated export is unchanged.");
    let output_path = temp.path().join("rebuilt.rbxl");
    let output = Command::new(ROJO_PATH)
        .args(["--color", "never", "build"])
        .arg(&copied_project)
        .arg("--output")
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Build failed. {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rebuilt = rbx_binary::from_reader(fs_err::File::open(&output_path).unwrap()).unwrap();
    let actual = scripts(&rebuilt, &ignored);
    if !expected.keys().eq(actual.keys()) {
        let missing = expected
            .keys()
            .filter(|path| !actual.contains_key(*path))
            .take(20)
            .collect::<Vec<_>>();
        let extra = actual
            .keys()
            .filter(|path| !expected.contains_key(*path))
            .take(20)
            .collect::<Vec<_>>();
        println!(
            "Preserved the isolated diagnostic copy at {}",
            temp.keep().display()
        );
        panic!("The rebuilt script hierarchy differs. Missing {missing:?}. Extra {extra:?}.");
    }
    for (path, records) in &expected {
        if actual[path] != *records {
            println!(
                "Preserved the isolated diagnostic copy at {}",
                temp.keep().display()
            );
            panic!("Script class, RunContext, or source differs at {path}");
        }
    }
    println!("Verified all rebuilt script classes, execution contexts, sources, and paths.");
    let metadata_before = first
        .0
        .keys()
        .filter(|path| path.ends_with(".json"))
        .map(|path| (path.clone(), fs_err::read(source.join(path)).unwrap()))
        .collect::<BTreeMap<_, _>>();
    let rebuilt_export = sync(&output_path);
    if rebuilt_export != first {
        let mut paths = first
            .0
            .keys()
            .chain(rebuilt_export.0.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        let changed = paths
            .into_iter()
            .filter(|path| first.0.get(path) != rebuilt_export.0.get(path))
            .collect::<Vec<_>>();
        assert_eq!(
            rebuilt_export.1, first.1,
            "The project must remain unchanged"
        );
        assert!(first.0.keys().eq(rebuilt_export.0.keys()));
        for path in &changed {
            let before = jsonc_parser::parse_to_serde_value(
                std::str::from_utf8(
                    metadata_before
                        .get(path)
                        .expect("Script source files must remain unchanged"),
                )
                .unwrap(),
                &Default::default(),
            )
            .unwrap()
            .unwrap();
            let after = jsonc_parser::parse_to_serde_value(
                &fs_err::read_to_string(source.join(path)).unwrap(),
                &Default::default(),
            )
            .unwrap()
            .unwrap();
            assert!(
                binary_rotation_changes_only(&before, &after, false),
                "Unexpected rebuilt metadata change at {path}"
            );
        }
        println!(
            "Binary serialization normalized rotation values in {} non-script metadata files.",
            changed.len()
        );
        assert_eq!(
            sync(&output_path),
            rebuilt_export,
            "Rebuilt exports must stabilize"
        );
    }
    let focus_records = &expected[&focus_path];
    assert_eq!(focus_records.len(), 1);
    assert_eq!(focus_records[0].0, "LocalScript");
    let exported = source
        .join(&focus_path)
        .with_file_name(format!("{focus}.local.luau"));
    assert!(
        exported.is_file(),
        "The real LocalScript must use its canonical suffix"
    );
    assert!(!exported
        .with_file_name(format!("{focus}.client.luau"))
        .exists());
    assert_eq!(
        manifest(&original_source),
        before_source,
        "Original source files changed"
    );
    assert_eq!(
        fs_err::read(&original_project).unwrap(),
        project_bytes,
        "The original project changed"
    );
    assert_eq!(
        digest(&original_place),
        before_place,
        "The original place changed"
    );
    println!(
        "Verified {} scripts across {} paths and {} exported files.",
        expected.values().map(Vec::len).sum::<usize>(),
        expected.len(),
        first.0.len()
    );
    println!("Verified the focus LocalScript at \"{focus_path}\" uses a local suffix.");
    println!("Verified repeated syncback, rebuild, script sources, execution contexts, hierarchy, and unchanged originals.");
}

/// Syncing a place again must only change files for what actually changed.
///
/// Instances a reference points at need a `Rojo_Id`. Two bugs made later runs
/// rewrite them. An instance with no `UniqueId` got a new made-up ID every run,
/// and rewriting an instance's metadata added an `id` next to the `Rojo_Id` it
/// was read from.
#[test]
fn syncing_back_again_only_changes_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs_err::create_dir_all(project.join("src")).unwrap();
    fs_err::write(
        project.join("default.project.json"),
        r#"{
  "name": "Rerun",
  "syncbackRules": { "instancesAsDirectories": true },
  "tree": {
    "$className": "DataModel",
    "ReplicatedStorage": { "$path": "src" }
  }
}"#,
    )
    .unwrap();

    let mut place = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("DataModel"));
    let storage = place.insert(
        place.root_ref(),
        rbx_dom_weak::InstanceBuilder::new("ReplicatedStorage").with_name("ReplicatedStorage"),
    );
    let model = place.insert(
        storage,
        rbx_dom_weak::InstanceBuilder::new("Model").with_name("Rig"),
    );
    let head = place.insert(
        model,
        rbx_dom_weak::InstanceBuilder::new("Part").with_name("Head"),
    );
    place
        .get_by_ref_mut(model)
        .unwrap()
        .properties
        .insert(ustr("PrimaryPart"), Variant::Ref(head));

    // A referenced script with children is written as a directory with
    // init.luau and an init.meta.json holding its Rojo_Id.
    let module = place.insert(
        storage,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Gradients")
            .with_property("Source", "return {}"),
    );
    place.insert(
        module,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Palette")
            .with_property("Source", "return {}"),
    );
    place.insert(
        storage,
        rbx_dom_weak::InstanceBuilder::new("ObjectValue")
            .with_name("Pointer")
            .with_property("Value", Variant::Ref(module)),
    );

    // Same-named scripts written differently, as `Twin.luau` and `Twin.__2/`, used
    // to load back in reverse and trade IDs on every run.
    let twin_file = place.insert(
        storage,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Twin")
            .with_property("Source", "return 1"),
    );
    let twin_dir = place.insert(
        storage,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Twin")
            .with_property("Source", "return 2"),
    );
    place.insert(
        twin_dir,
        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
            .with_name("Child")
            .with_property("Source", "return 3"),
    );
    for (name, target) in [("ToFirstTwin", twin_file), ("ToSecondTwin", twin_dir)] {
        place.insert(
            storage,
            rbx_dom_weak::InstanceBuilder::new("ObjectValue")
                .with_name(name)
                .with_property("Value", Variant::Ref(target)),
        );
    }
    let input = dir.path().join("place.rbxl");
    let sync = |place: &WeakDom| {
        let file = std::io::BufWriter::new(fs_err::File::create(&input).unwrap());
        rbx_binary::to_writer(file, place, &[storage]).unwrap();
        let status = Command::new(ROJO_PATH)
            .args(["--color", "never", "syncback"])
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("-y")
            .status()
            .unwrap();
        assert!(status.success());
        read_files(&project)
    };

    let first = sync(&place);
    assert!(
        first
            .values()
            .any(|contents| contents.contains("\"Rojo_Id\"")),
        "the referenced instances should have been given a Rojo_Id"
    );

    // The same place again changes nothing.
    pretty_assertions::assert_eq!(sync(&place), first);

    // Editing the referenced script rewrites only its source.
    place.get_by_ref_mut(module).unwrap().properties.insert(
        ustr("Source"),
        Variant::String("return { edited = true }".into()),
    );
    let mut expected = first.clone();
    expected.insert(
        "src/Gradients/init.luau".to_owned(),
        "return { edited = true }".to_owned(),
    );
    pretty_assertions::assert_eq!(sync(&place), expected);
}

/// Exercise every adjacent and initializer writer with the same metadata contract.
fn check_jsonc_metadata_writer(
    extension: &str,
    initializer: bool,
    old_name: &str,
    new_name: &str,
    stem: &str,
    clearing: bool,
    dual: bool,
) {
    use rbx_dom_weak::InstanceBuilder;

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("default.project.json");
    let src = dir.path().join("src");
    fs_err::create_dir_all(&src).unwrap();
    fs_err::write(
        &project,
        r#"{"name":"Metadata","syncbackRules":{"instancesAsDirectories":true},
        "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
    )
    .unwrap();
    let (source_path, meta_path) = if initializer {
        let folder = src.join(stem);
        fs_err::create_dir(&folder).unwrap();
        (
            folder.join(format!("init.{extension}")),
            folder.join("init.meta.jsonc"),
        )
    } else {
        (
            src.join(format!("{stem}.{extension}")),
            src.join(format!("{stem}.meta.jsonc")),
        )
    };
    let (class, property, original, incoming) = match extension {
        "luau" => ("ModuleScript", "Source", "return 'old'", "return 'new'"),
        "txt" => ("StringValue", "Value", "old", "new"),
        "csv" => (
            "LocalizationTable",
            "Contents",
            "Key,Source\nGreeting,Old\n",
            r#"[{"key":"Greeting","source":"New","values":{}}]"#,
        ),
        _ => unreachable!(),
    };
    fs_err::write(&source_path, original).unwrap();
    let mut meta = serde_json::json!({"name":old_name,
        "properties":{"Archivable":false}, "attributes":{"Stale":"old"}});
    let preserve_identity = !clearing && old_name == "a?";
    if preserve_identity {
        meta["id"] = "owner".into();
        meta["$schema"] = "schema.json".into();
        meta["ignoreUnknownInstances"] = true.into();
    }
    // A real JSONC file, not just JSON with a different extension.
    fs_err::write(&meta_path, format!("// Existing metadata.\n{meta}\n")).unwrap();
    if dual {
        fs_err::write(meta_path.with_extension("json"), "{}").unwrap();
    }
    let mut child = InstanceBuilder::new(class)
        .with_name(new_name)
        .with_property(property, incoming);
    if initializer {
        // Initially inline children must be materialized once, never resurrected
        // from a stale second metadata file on a later build.
        meta["children"] = serde_json::json!([{"className":"Folder", "name":"Retained"}]);
        fs_err::write(&meta_path, format!("// Existing metadata.\n{meta}\n")).unwrap();
        child = child.with_child(InstanceBuilder::new("Folder").with_name("Retained"));
    }
    let dom = WeakDom::new(
        InstanceBuilder::new("DataModel")
            .with_child(InstanceBuilder::new("ReplicatedStorage").with_child(child)),
    );
    let input = dir.path().join("incoming.rbxl");
    rbx_binary::to_writer(
        fs_err::File::create(&input).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    let before = read_files(&src);
    let mut previous = None;
    for _ in 0..2 {
        let result = Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .arg("--non-interactive")
            .output()
            .unwrap();
        if dual {
            assert!(
                !result.status.success(),
                "Dual metadata must fail before persistence for {source_path:?}."
            );
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("hold this instance's metadata"),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(read_files(&src), before);
            return;
        }
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output = dir.path().join("rebuilt.rbxl");
        let result = Command::new(ROJO_PATH)
            .arg("build")
            .arg(&project)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rebuilt = rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap();
        let child = rebuilt
            .descendants()
            .find(|child| child.class == class)
            .unwrap();
        assert_eq!(child.name, new_name, "{source_path:?}");
        assert_eq!(
            child.properties[&ustr(property)],
            Variant::String(incoming.into())
        );
        assert_ne!(
            child.properties.get(&ustr("Archivable")),
            Some(&Variant::Bool(false))
        );
        assert!(!matches!(child.properties.get(&ustr("Attributes")),
            Some(Variant::Attributes(attributes)) if attributes.get("Stale").is_some()));
        if initializer {
            assert_eq!(child.children().len(), 1);
            assert_eq!(
                rebuilt.get_by_ref(child.children()[0]).unwrap().name,
                "Retained"
            );
        }
        assert!(!meta_path.with_extension("json").exists());
        let updated: serde_json::Value =
            serde_json::from_slice(&fs_err::read(&meta_path).unwrap()).unwrap();
        if clearing {
            assert_eq!(updated, serde_json::json!({}));
        } else {
            assert_eq!(updated["name"], new_name);
            if preserve_identity {
                assert_eq!(updated["id"], "owner");
                assert_eq!(updated["$schema"], "schema.json");
                assert_eq!(updated["ignoreUnknownInstances"], true);
            }
        }
        let files = read_files(&src);
        if let Some(previous) = previous.replace(files.clone()) {
            assert_eq!(files, previous);
        }
    }
}

#[test]
fn jsonc_metadata_writers_round_trip_normalized_renames() {
    for (extension, initializer) in [
        ("luau", false),
        ("luau", true),
        ("txt", false),
        ("csv", false),
        ("csv", true),
    ] {
        for (old, new, stem) in [("Twin", "twin", "Twin"), ("a?", "a*", "a_")] {
            check_jsonc_metadata_writer(extension, initializer, old, new, stem, false, false);
        }
    }
}

#[test]
fn jsonc_metadata_writers_clear_removed_values() {
    for (extension, initializer) in [
        ("luau", false),
        ("luau", true),
        ("txt", false),
        ("csv", false),
        ("csv", true),
    ] {
        check_jsonc_metadata_writer(extension, initializer, "Twin", "Twin", "Twin", true, false);
    }
}

#[test]
fn jsonc_metadata_writers_reject_dual_files_without_persistence() {
    for (extension, initializer) in [
        ("luau", false),
        ("luau", true),
        ("txt", false),
        ("csv", false),
        ("csv", true),
    ] {
        check_jsonc_metadata_writer(extension, initializer, "Twin", "twin", "Twin", false, true);
        check_jsonc_metadata_writer(extension, initializer, "a?", "a*", "a_", false, true);
        check_jsonc_metadata_writer(extension, initializer, "Twin", "Twin", "Twin", true, true);
    }
}

fn release_review_project(root: &Path, rules: serde_json::Value) -> std::path::PathBuf {
    let project = root.join("default.project.json");
    fs_err::write(
        &project,
        serde_json::to_vec(&serde_json::json!({
            "name":"Review", "syncbackRules":rules,
            "tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}
        }))
        .unwrap(),
    )
    .unwrap();
    project
}

fn release_review_input(
    root: &Path,
    children: Vec<rbx_dom_weak::InstanceBuilder>,
) -> std::path::PathBuf {
    use rbx_dom_weak::InstanceBuilder;
    let dom = WeakDom::new(
        InstanceBuilder::new("DataModel")
            .with_child(InstanceBuilder::new("ReplicatedStorage").with_children(children)),
    );
    let input = root.join("incoming.rbxl");
    rbx_binary::to_writer(
        fs_err::File::create(&input).unwrap(),
        &dom,
        dom.root().children(),
    )
    .unwrap();
    input
}

fn release_review_sync(project: &Path, input: &Path) -> std::process::Output {
    Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(project)
        .arg("--input")
        .arg(input)
        .arg("--non-interactive")
        .output()
        .unwrap()
}

fn release_review_build(project: &Path) -> WeakDom {
    let output = project.parent().unwrap().join("rebuilt.rbxl");
    let result = Command::new(ROJO_PATH)
        .arg("build")
        .arg(project)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    rbx_binary::from_reader(fs_err::File::open(output).unwrap()).unwrap()
}

fn release_review_bytes(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs_err::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

/// Both sides of a CSV initializer class change need destructive-layout validation.
#[test]
fn release_review_csv_initializer_class_changes() {
    use rbx_dom_weak::InstanceBuilder;
    for reverse in [false, true] {
        for ignored in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("src/Twin");
            fs_err::create_dir_all(source.join("Stale")).unwrap();
            fs_err::write(source.join("keep.txt"), "original bytes").unwrap();
            if !reverse {
                fs_err::write(source.join("init.csv"), "Key,Source,en\nold,Old,Old\n").unwrap();
            }
            let project = release_review_project(
                dir.path(),
                serde_json::json!({
                    "instancesAsDirectories":true,
                    "ignorePaths":if ignored { vec!["**/keep.txt"] } else { vec![] }
                }),
            );
            let class = if reverse {
                "LocalizationTable"
            } else {
                "Folder"
            };
            let mut child = InstanceBuilder::new(class).with_name("Twin").with_child(
                InstanceBuilder::new("ModuleScript")
                    .with_name("Child")
                    .with_property("Source", "return 8"),
            );
            if reverse {
                child = child.with_property(
                    "Contents",
                    r#"[{"key":"new","source":"New","values":{"en":"New"}}]"#,
                );
            }
            let input = release_review_input(dir.path(), vec![child]);
            let before = release_review_bytes(&source);
            let mut previous = None;
            for _ in 0..2 {
                let result = release_review_sync(&project, &input);
                assert_eq!(
                    result.status.success(),
                    !ignored,
                    "reverse {reverse}, {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                if ignored {
                    assert!(String::from_utf8_lossy(&result.stderr).contains("ignorePaths"));
                    assert_eq!(release_review_bytes(&source), before);
                    break;
                }
                assert_eq!(source.join("init.csv").is_file(), reverse);
                assert!(!source.join("Stale").exists());
                assert!(!source.join("keep.txt").exists());
                let rebuilt = release_review_build(&project);
                let child = rebuilt
                    .descendants()
                    .find(|inst| inst.name == "Twin")
                    .unwrap();
                assert_eq!(child.class.as_str(), class);
                assert_eq!(child.children().len(), 1);
                assert_eq!(
                    rebuilt.get_by_ref(child.children()[0]).unwrap().name,
                    "Child"
                );
                if reverse {
                    assert!(
                        matches!(&child.properties[&ustr("Contents")], Variant::String(contents) if contents.contains("New") && !contents.contains("Old"))
                    );
                }
                let bytes = release_review_bytes(&source);
                if let Some(previous) = previous.replace(bytes.clone()) {
                    assert_eq!(bytes, previous);
                }
            }
        }
    }
}

/// Byte-identical metadata must be restaged when its containing directory is removed.
#[test]
fn release_review_identical_root_metadata_survives_relayout() {
    use rbx_dom_weak::InstanceBuilder;
    for extension in ["json", "jsonc"] {
        for (identity, schema) in [(true, true), (true, false), (false, true)] {
            for ignore in [Some(true), None, Some(false)] {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("src/Twin");
                fs_err::create_dir_all(&source).unwrap();
                fs_err::write(source.join("init.luau"), "return 'stale'").unwrap();
                // Match DirectoryMetadata's serialization order and whitespace exactly.
                let mut fields = Vec::new();
                if schema {
                    fields.push("    \"$schema\": \"schema.json\"");
                }
                if identity {
                    fields.push("    \"id\": \"owner\"");
                }
                let mut expected = fields.clone();
                if ignore == Some(true) {
                    expected.push("    \"ignoreUnknownInstances\": true");
                }
                match ignore {
                    Some(true) => fields.push("    \"ignoreUnknownInstances\": true"),
                    Some(false) => fields.push("    \"ignoreUnknownInstances\": false"),
                    None => {}
                }
                let contents = format!("{{\n{}\n}}\n", fields.join(",\n"));
                let expected = format!("{{\n{}\n}}\n", expected.join(",\n"));
                let meta = source.join(format!("init.meta.{extension}"));
                fs_err::write(&meta, &contents).unwrap();
                if ignore != Some(false) {
                    assert_eq!(contents, expected);
                }
                let project = release_review_project(
                    dir.path(),
                    serde_json::json!({"instancesAsDirectories":true}),
                );
                let input = release_review_input(
                    dir.path(),
                    vec![InstanceBuilder::new("Folder").with_name("Twin").with_child(
                        InstanceBuilder::new("ModuleScript")
                            .with_name("Child")
                            .with_property("Source", "return 8"),
                    )],
                );
                let mut previous = None;
                for _ in 0..2 {
                    let result = release_review_sync(&project, &input);
                    assert!(
                        result.status.success(),
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    assert_eq!(
                        fs_err::read_to_string(&meta).unwrap(),
                        expected,
                        "{extension}, identity {identity}, schema {schema}, ignore {ignore:?}"
                    );
                    assert!(!source.join("init.luau").exists());
                    assert!(!source
                        .join(format!(
                            "init.meta.{}",
                            if extension == "json" { "jsonc" } else { "json" }
                        ))
                        .exists());
                    let rebuilt = release_review_build(&project);
                    let child = rebuilt
                        .descendants()
                        .find(|inst| inst.name == "Twin")
                        .unwrap();
                    assert_eq!(child.class.as_str(), "Folder");
                    assert_eq!(child.children().len(), 1);
                    assert!(!child.properties.contains_key(&ustr("Source")));
                    let bytes = release_review_bytes(&source);
                    if let Some(previous) = previous.replace(bytes.clone()) {
                        assert_eq!(bytes, previous);
                    }
                }
            }
        }
    }
}

fn release_review_twin(name: &str, number: u64) -> rbx_dom_weak::InstanceBuilder {
    use rbx_dom_weak::InstanceBuilder;
    InstanceBuilder::new("Folder").with_name(name).with_child(
        InstanceBuilder::new("ModuleScript")
            .with_name("Marker")
            .with_property("Source", format!("return {number}")),
    )
}

fn release_review_old_twin(
    src: &Path,
    stem: &str,
    name: &str,
    number: u64,
    layout: &str,
) -> std::path::PathBuf {
    let meta = if layout == "dir" {
        let path = src.join(stem);
        fs_err::create_dir_all(&path).unwrap();
        fs_err::write(path.join("Marker.luau"), format!("return {number}")).unwrap();
        path.join("init.meta.jsonc")
    } else {
        let path = src.join(format!("{stem}.{layout}"));
        let dom = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Folder")
                .with_child(release_review_twin(name, number)),
        );
        match layout {
            "rbxm" => rbx_binary::to_writer(fs_err::File::create(path).unwrap(), &dom, dom.root().children()).unwrap(),
            "rbxmx" => rbx_xml::to_writer_default(fs_err::File::create(path).unwrap(), &dom, dom.root().children()).unwrap(),
            "model.json" => fs_err::write(path, serde_json::to_vec(&serde_json::json!({"className":"Folder",
                "children":[{"className":"ModuleScript","name":"Marker","properties":{"Source":format!("return {number}")}}]})).unwrap()).unwrap(),
            _ => unreachable!()
        }
        src.join(format!("{stem}.meta.jsonc"))
    };
    fs_err::write(&meta, serde_json::to_vec(&serde_json::json!({"name":name, "id":format!("owner-{number}"), "$schema":format!("schema-{number}"), "ignoreUnknownInstances":true})).unwrap()).unwrap();
    meta
}

/// A replacement cannot silently jump to another ordinal to avoid either sidecar.
#[test]
fn release_review_paired_relayout_conflicts_preserve_sources() {
    use rbx_dom_weak::InstanceBuilder;
    for marker in [".__", "~"] {
        for blocker in [
            "disk-json",
            "disk-jsonc",
            "pending-json",
            "pending-jsonc",
            "retained",
            "ignored",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("src");
            fs_err::create_dir_all(&src).unwrap();
            release_review_old_twin(&src, &format!("Twin{marker}3"), "Twin", 3, "rbxm");
            release_review_old_twin(&src, &format!("Twin{marker}8"), "Twin", 8, "dir");
            let mut incoming = vec![
                release_review_twin("Twin", 3),
                release_review_twin("Twin", 8),
            ];
            match blocker {
                "disk-json" | "disk-jsonc" => {
                    let extension = if blocker == "disk-json" {
                        "json"
                    } else {
                        "jsonc"
                    };
                    fs_err::write(src.join(format!("TWIN.__8.meta.{extension}")), "{}").unwrap();
                }
                "pending-json" | "pending-jsonc" => {
                    let extension = if blocker == "pending-json" {
                        "json"
                    } else {
                        "jsonc"
                    };
                    incoming.push(
                        InstanceBuilder::new("Folder")
                            .with_name(format!("twin.__8.meta.{extension}")),
                    );
                }
                "retained" => {
                    let path = src.join("Twin.__8.rbxm");
                    fs_err::create_dir(&path).unwrap();
                    fs_err::write(path.join("init.meta.json"), r#"{"id":"unrelated"}"#).unwrap();
                    incoming.push(InstanceBuilder::new("Folder").with_name("Twin.__8.rbxm"));
                }
                "ignored" => {
                    fs_err::write(src.join(format!("Twin{marker}8/keep.txt")), "ignored bytes")
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let project = release_review_project(
                dir.path(),
                serde_json::json!({
                    "instancesAsDirectories":true,
                    "modelTrees":[{"pattern":"ReplicatedStorage/Twin"}],
                    "ignorePaths":if blocker == "ignored" { vec!["**/keep.txt"] } else { vec![] }
                }),
            );
            let input = release_review_input(dir.path(), incoming);
            let before = release_review_bytes(&src);
            let result = release_review_sync(&project, &input);
            assert!(!result.status.success(), "{marker}, {blocker}");
            let error = String::from_utf8_lossy(&result.stderr);
            assert!(
                error.contains(if blocker == "ignored" {
                    "ignorePaths"
                } else {
                    "another source owns its destination"
                }),
                "{marker}, {blocker}, {error}"
            );
            assert_eq!(release_review_bytes(&src), before);
            assert!(!dir.path().join("src.rbxm").exists());
        }
    }
}

/// Reusing a paired directory as a model at the same path has a proven owner.
#[test]
fn release_review_paired_relayout_can_reuse_its_removed_source() {
    for name in ["Twin", "Twin.__100"] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs_err::create_dir_all(&src).unwrap();
        let first_number = if name == "Twin" { 3 } else { 1 };
        let first_stem = if first_number == 1 {
            name.to_owned()
        } else {
            format!("{name}.__3")
        };
        release_review_old_twin(&src, &first_stem, name, first_number, "rbxmx");
        release_review_old_twin(&src, &format!("{name}.__8.rbxm"), name, 8, "dir");
        let project = release_review_project(
            dir.path(),
            serde_json::json!({"instancesAsDirectories":true, "modelTrees":[{"pattern":"ReplicatedStorage/*"}]}),
        );
        let input = release_review_input(
            dir.path(),
            vec![
                release_review_twin(name, first_number),
                release_review_twin(name, 8),
            ],
        );
        let mut previous = None;
        for _ in 0..2 {
            let result = release_review_sync(&project, &input);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(src.join(format!("{name}.__8.rbxm")).is_file());
            let rebuilt = release_review_build(&project);
            let storage = rebuilt
                .descendants()
                .find(|inst| inst.class == "ReplicatedStorage")
                .unwrap();
            let sources: Vec<_> = storage
                .children()
                .iter()
                .map(|id| {
                    let child = rebuilt.get_by_ref(*id).unwrap();
                    rebuilt.get_by_ref(child.children()[0]).unwrap().properties[&ustr("Source")]
                        .clone()
                })
                .collect();
            assert_eq!(
                sources,
                [first_number, 8].map(|number| Variant::String(format!("return {number}")))
            );
            assert!(src.join(format!("{first_stem}.rbxm")).is_file());
            let bytes = release_review_bytes(&src);
            if let Some(previous) = previous.replace(bytes.clone()) {
                assert_eq!(bytes, previous);
            }
        }
    }
}

/// Paired relayouts and genuine additions share the same sparse ordinal floor.
#[test]
fn release_review_sparse_relayout_appends_after_pending_sidecars() {
    use rbx_dom_weak::InstanceBuilder;
    for marker in [".__", "~"] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs_err::create_dir_all(&src).unwrap();
        release_review_old_twin(&src, &format!("Twin{marker}3"), "Twin", 3, "rbxm");
        release_review_old_twin(&src, &format!("Twin{marker}8"), "Twin", 8, "dir");
        let project = release_review_project(
            dir.path(),
            serde_json::json!({"instancesAsDirectories":true,
            "modelTrees":[{"pattern":"ReplicatedStorage/Twin"}]}),
        );
        let mut incoming: Vec<_> = [3, 8, 10, 11]
            .into_iter()
            .map(|number| release_review_twin("Twin", number))
            .collect();
        incoming.push(InstanceBuilder::new("Folder").with_name("TWIN.__9.meta.jsonc"));
        let input = release_review_input(dir.path(), incoming);
        let mut previous = None;
        for _ in 0..2 {
            let result = release_review_sync(&project, &input);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(src.join("Twin.__8.rbxm").is_file());
            for number in [10, 11] {
                assert!(src.join(format!("Twin.__{number}.rbxm")).is_file());
            }
            assert!(!src.join("Twin.__9.rbxm").exists());
            let rebuilt = release_review_build(&project);
            let storage = rebuilt
                .descendants()
                .find(|inst| inst.class == "ReplicatedStorage")
                .unwrap();
            let sources: Vec<_> = storage
                .children()
                .iter()
                .filter_map(|id| {
                    let child = rebuilt.get_by_ref(*id).unwrap();
                    (child.name == "Twin").then(|| {
                        rebuilt.get_by_ref(child.children()[0]).unwrap().properties[&ustr("Source")]
                            .clone()
                    })
                })
                .collect();
            assert_eq!(
                sources,
                [3, 8, 10, 11].map(|number| Variant::String(format!("return {number}")))
            );
            let bytes = release_review_bytes(&src);
            if let Some(previous) = previous.replace(bytes.clone()) {
                assert_eq!(bytes, previous);
            }
            rbx_binary::to_writer(
                fs_err::File::create(&input).unwrap(),
                &rebuilt,
                rebuilt.root().children(),
            )
            .unwrap();
        }
    }
}

/// Sparse physical ordinals are identities, not current child-vector positions.
#[test]
fn release_review_sparse_paired_relayouts_preserve_order() {
    for (first_layout, second_layout, target) in [
        ("rbxm", "dir", Some("rbxm")),
        ("rbxmx", "dir", Some("rbxmx")),
        ("model.json", "dir", Some("rbxm")),
        ("rbxm", "rbxmx", Some("rbxmx")),
        ("rbxm", "rbxm", None),
        ("dir", "rbxm", None),
    ] {
        for marker in [".__", "~"] {
            for name in ["Twin", "Twin.__100"] {
                let dir = tempfile::tempdir().unwrap();
                let src = dir.path().join("src");
                fs_err::create_dir_all(&src).unwrap();
                let second_name = name.to_lowercase();
                let stems = [
                    format!("{name}{marker}3"),
                    format!("{}{marker}8", name.to_uppercase()),
                ];
                let old_meta = [
                    release_review_old_twin(&src, &stems[0], name, 3, first_layout),
                    release_review_old_twin(&src, &stems[1], &second_name, 8, second_layout),
                ];
                let mut rules = serde_json::json!({"instancesAsDirectories":true});
                if let Some(target) = target {
                    rules["modelTrees"] =
                        serde_json::json!([{"pattern":"ReplicatedStorage/*", "format":target}]);
                }
                let project = release_review_project(dir.path(), rules);
                let input = release_review_input(
                    dir.path(),
                    vec![
                        release_review_twin(name, 3),
                        release_review_twin(&second_name, 8),
                    ],
                );
                let mut previous = None;
                for _ in 0..2 {
                    let result = release_review_sync(&project, &input);
                    assert!(
                        result.status.success(),
                        "{first_layout}, {second_layout}, {target:?}, {marker}, {name}, {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    let rebuilt = release_review_build(&project);
                    let storage = rebuilt
                        .descendants()
                        .find(|inst| inst.class == "ReplicatedStorage")
                        .unwrap();
                    let sources: Vec<_> = storage
                        .children()
                        .iter()
                        .map(|id| {
                            let root = rebuilt.get_by_ref(*id).unwrap();
                            assert_eq!(root.class.as_str(), "Folder");
                            assert_eq!(root.children().len(), 1);
                            rebuilt.get_by_ref(root.children()[0]).unwrap().properties
                                [&ustr("Source")]
                                .clone()
                        })
                        .collect();
                    assert_eq!(
                        sources,
                        [3, 8].map(|number| Variant::String(format!("return {number}")))
                    );
                    for (index, (logical, layout, number)) in [
                        (name, first_layout, 3),
                        (second_name.as_str(), second_layout, 8),
                    ]
                    .iter()
                    .enumerate()
                    {
                        let retained =
                            target == Some(*layout) || (target.is_none() && *layout == "dir");
                        let stem = if retained {
                            stems[index].clone()
                        } else {
                            format!("{logical}.__{number}")
                        };
                        let meta = if let Some(target) = target {
                            assert!(src.join(format!("{stem}.{target}")).is_file());
                            if retained {
                                old_meta[index].clone()
                            } else {
                                let json = src.join(format!("{stem}.meta.json"));
                                if json.is_file() {
                                    json
                                } else {
                                    json.with_extension("jsonc")
                                }
                            }
                        } else {
                            assert!(src.join(&stem).is_dir());
                            if retained {
                                old_meta[index].clone()
                            } else {
                                src.join(&stem).join("init.meta.json")
                            }
                        };
                        let metadata = jsonc_parser::parse_to_serde_value(
                            &fs_err::read_to_string(&meta).unwrap(),
                            &Default::default(),
                        )
                        .unwrap()
                        .unwrap();
                        assert_eq!(metadata["id"], format!("owner-{number}"));
                        assert_eq!(metadata["$schema"], format!("schema-{number}"));
                        assert_eq!(metadata["ignoreUnknownInstances"], true);
                        if old_meta[index] != meta {
                            assert!(!old_meta[index].exists());
                        }
                        let old_source = if *layout == "dir" {
                            src.join(&stems[index])
                        } else {
                            src.join(format!("{}.{layout}", stems[index]))
                        };
                        let output = if let Some(target) = target {
                            src.join(format!("{stem}.{target}"))
                        } else {
                            src.join(&stem)
                        };
                        if old_source != output {
                            assert!(!old_source.exists());
                        }
                    }
                    let bytes = release_review_bytes(&src);
                    if let Some(previous) = previous.replace(bytes.clone()) {
                        assert_eq!(bytes, previous);
                    }
                    rbx_binary::to_writer(
                        fs_err::File::create(&input).unwrap(),
                        &rebuilt,
                        rebuilt.root().children(),
                    )
                    .unwrap();
                }
            }
        }
    }
}
