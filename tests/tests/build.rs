use std::{fs, path::Path, process::Command};

use insta::assert_snapshot;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, BUILD_TESTS_PATH, ROJO_PATH};

macro_rules! gen_build_tests {
    ( $($test_name: ident,)* ) => {
        $(
            paste::item! {
                #[test]
                fn [<build_ $test_name>]() {
                    let _ = env_logger::try_init();

                    run_build_test(stringify!($test_name));
                }
            }
        )*
    };
}

gen_build_tests! {
    init_csv_with_children,
    attributes,
    client_in_folder,
    client_init,
    csv_bug_145,
    csv_bug_147,
    csv_in_folder,
    deep_nesting,
    gitkeep,
    ignore_glob_inner,
    ignore_glob_nested,
    ignore_glob_spec,
    infer_service_name,
    infer_starter_player,
    init_meta_class_name,
    init_meta_properties,
    init_with_children,
    issue_546,
    json_as_lua,
    json_model_in_folder,
    json_model_legacy_name,
    module_in_folder,
    module_init,
    nested_runcontext,
    optional,
    project_composed_default,
    project_composed_file,
    project_root_name,
    rbxm_in_folder,
    rbxmx_in_folder,
    rbxmx_ref,
    script_meta_disabled,
    server_in_folder,
    server_init,
    txt,
    txt_in_folder,
    unresolved_values,
    weldconstraint,
    sync_rule_alone,
    sync_rule_complex,
    sync_rule_nested_projects,
    no_name_default_project,
    no_name_project,
    no_name_top_level_project,
    plugin_init,
}

#[test]
fn parallel_build_matches_serial_build() {
    let input = tempdir().unwrap();
    fs::write(
        input.path().join("default.project.json"),
        r#"{
        "name": "ParallelBuildTest",
        "tree": { "$className": "DataModel", "ReplicatedStorage": { "$path": "Source" } }
    }"#,
    )
    .unwrap();
    for group in 0..16 {
        let directory = input.path().join(format!("Source/Group{group:02}"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("init.meta.json"),
            r#"{ "className": "Model" }"#,
        )
        .unwrap();
        for child in 0..16 {
            fs::write(
                directory.join(format!("Child{child:02}.luau")),
                format!("return {group} * 16 + {child}\n"),
            )
            .unwrap();
        }
    }
    let mut expected = None;
    for workers in [1, 2, 4] {
        let output_path = input.path().join(format!("output-{workers}.rbxl"));
        let output = Command::new(ROJO_PATH)
            .arg("build")
            .arg(input.path())
            .arg("--jobs")
            .arg(workers.to_string())
            .arg("--output")
            .arg(&output_path)
            .current_dir(get_working_dir_path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = fs::read(output_path).unwrap();
        match &expected {
            None => expected = Some(bytes),
            Some(expected) => assert_eq!(*expected, bytes),
        }
    }
}

#[test]
fn build_rejects_zero_workers() {
    let output = Command::new(ROJO_PATH)
        .args(["build", "--jobs", "0", "-o", "unused.rbxl"])
        .current_dir(get_working_dir_path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--jobs"));
}

#[test]
fn automated_release_uses_the_exact_unprefixed_version_tag() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../.github/workflows/release.yaml")).unwrap();
    let steps = workflow["jobs"]["release-github"]["steps"]
        .as_sequence()
        .unwrap();
    let publish = steps
        .iter()
        .find(|step| step["name"].as_str() == Some("Create release"))
        .unwrap()["run"]
        .as_str()
        .unwrap();
    assert!(publish.contains("gh release create \"${{ needs.init.outputs.version }}\""));
    assert!(publish.contains("--target \"${{ github.sha }}\""));
    #[cfg(unix)]
    {
        let script = publish
            .replace("${{ needs.init.outputs.version }}", "1.2.3")
            .replace("${{ github.sha }}", "owned-build");
        let output = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "gh() {{ [ \"$1\" = release ] && [ \"$2\" = create ] && [ \"$4\" = --target ] && [ \"$5\" = owned-build ] || return 2; printf '%s' \"$3\"; }}\n{script}"
            ))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"1.2.3");
    }
}

#[test]
fn automated_release_rejects_existing_unprefixed_and_historical_tags() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../.github/workflows/release-checks.yaml")).unwrap();
    let steps = workflow["jobs"]["version"]["steps"].as_sequence().unwrap();
    let validation = steps
        .iter()
        .find(|step| step["name"].as_str() == Some("Check version bump"))
        .unwrap()["run"]
        .as_str()
        .unwrap();
    assert!(validation.contains("for TAG in \"$VERSION\" \"v$VERSION\""));
    assert!(validation.contains("\"refs/tags/$TAG\""));
    #[cfg(unix)]
    {
        let root = tempdir().unwrap();
        fs::write(root.path().join("VERSION"), "1.2.3\n").unwrap();
        let git = r#"git() {
            [ "$1" = fetch ] && return 0
            for argument in "$@"; do
                [ "$argument" = "$EXPECTED_TAG" ] && return 0
            done
            return 1
        }"#;
        for existing in ["", "refs/tags/1.2.3", "refs/tags/v1.2.3"] {
            let output = Command::new("bash")
                .arg("-c")
                .arg(format!("{git}\n{validation}"))
                .current_dir(root.path())
                .env("EXPECTED_TAG", existing)
                .output()
                .unwrap();
            assert_eq!(output.status.success(), existing.is_empty(), "{existing}");
        }
    }
}

fn run_build_test(test_name: &str) {
    let working_dir = get_working_dir_path();

    let input_path = Path::new(BUILD_TESTS_PATH).join(test_name);

    let output_dir = tempdir().expect("couldn't create temporary directory");
    let output_path = output_dir.path().join(format!("{}.rbxmx", test_name));

    let output = Command::new(ROJO_PATH)
        .args([
            "build",
            input_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(working_dir)
        .output()
        .expect("Couldn't start Rojo");

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    assert!(output.status.success(), "Rojo did not exit successfully");

    let contents = fs::read_to_string(&output_path).expect("Couldn't read output file");

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(BUILD_TESTS_PATH)
        .parent()
        .unwrap()
        .join("build-test-snapshots");

    settings.set_snapshot_path(snapshot_path);

    settings.bind(|| {
        assert_snapshot!(test_name, contents);
    });
}
