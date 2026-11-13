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
    // Non-finite attributes are skipped so every JSON writer stays readable.
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
