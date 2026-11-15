use std::{ffi::OsStr, path::Path, process::Command};

use insta::assert_snapshot;
use rbx_dom_weak::{types::Variant, ustr, WeakDom};

use crate::rojo_test::{
    io_util::{ROJO_PATH, SYNCBACK_TESTS_PATH},
    syncback_util::{run_syncback_test, snapshot_rbxm},
};

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
    inline_layout_changes => ["src/Collapse/init.meta.json", "src/Expand/Holder/Added.server.luau", "src/Expand/Holder/Brick/init.meta.json"],
    // Ensures that script-free subtrees are written into one init.meta.json
    inline_script_free => ["src/Props/init.meta.json", "src/Mixed/Data/init.meta.json", "src/Mixed/Module.luau", "src/Mixed/Scripts/Marker/init.meta.json", "src/InPointer/init.meta.json"],
    // Ensures that all of the JSON middlewares are handled as expected
    json_middlewares => ["src/dir_with_meta/init.meta.json", "src/model_json.model.json", "src/project_json.project.json"],
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

    // Same-named scripts written differently, as `Twin.luau` and `Twin~2/`, used
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
