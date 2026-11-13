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
