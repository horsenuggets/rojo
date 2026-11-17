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
