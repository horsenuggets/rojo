//! Synthetic Wally v1 contract fixtures, independent of an installed Wally binary.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

pub const ORIGINAL: &str = "Source/ForkedPackages/foo/Source/Foo/init.luau";
pub const INSTALLED: &str = "Packages/_Index/scope_foo@local/foo";
pub const REGISTRY: &str = "Packages/_Index/scope_registry@1.2.3/registry";

pub fn setup(root: &Path) {
    for (path, contents) in [
        ("wally.toml", "[package]\nname = \"scope/app\"\nversion = \"1.0.0\"\nrealm = \"shared\"\n[dependencies]\nFoo = \"^Source/ForkedPackages/foo\"\nRegistry = \"scope/registry@1.2.3\"\n"),
        ("Source/ForkedPackages/foo/wally.toml", "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n"),
        ("Source/ForkedPackages/foo/default.project.json", r#"{"tree":{"$path":"Source/Foo"}}"#),
        (ORIGINAL, "return { Value = 1 }\n"),
        ("Source/ForkedPackages/foo/Source/Foo/Child.luau", "return 7\n"),
        ("Packages/_Index/scope_foo@local/foo/default.project.json", r#"{"tree":{"$path":"Source/Foo"}}"#),
        ("Packages/_Index/scope_foo@local/foo/Source/Foo/init.luau", "return { Value = -100 }\n"),
        ("Packages/_Index/scope_registry@1.2.3/registry/init.luau", "return 42\n"),
        ("Packages/Foo.luau", "return require(\"./_Index/scope_foo@local/foo\")\n"),
        ("Packages/Registry.luau", "return require(\"./_Index/scope_registry@1.2.3/registry\")\n"),
        (".luaurc", r#"{"aliases":{"packages":"Packages","shared":"Source/Shared"}}"#),
        ("Source/App.luau", "local x = require(\"@shared/Util\")\nreturn x\n"),
        ("Source/Shared/Util.luau", "return 9\n"),
        ("default.project.json", r#"{"name":"PackageFixture","tree":{"$path":"place.project.json"}}"#),
        ("place.project.json", r#"{"tree":{"$className":"DataModel","ReplicatedStorage":{"Packages":{"$path":"Packages"},"App":{"$path":"Source/App.luau"},"Shared":{"$path":"Source/Shared"}}}}"#),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let digest = |path| format!("{:x}", Sha256::digest(fs::read(root.join(path)).unwrap()));
    let install = json!({
        "version": 1,
        "manifest_hash": digest("wally.toml"),
        "local_manifests": { "Source/ForkedPackages/foo/wally.toml": digest("Source/ForkedPackages/foo/wally.toml") },
        "package_roots": [
            { "name":"scope/foo", "kind":"local", "realm":"shared", "install_path":INSTALLED, "source_path":"Source/ForkedPackages/foo" },
            { "name":"scope/registry", "kind":"registry", "realm":"shared", "install_path":REGISTRY, "package_version":"1.2.3" }
        ],
        "generated_paths":["Packages/Foo.luau", "Packages/Registry.luau"]
    });
    fs::write(
        root.join("wally.install.json"),
        serde_json::to_vec_pretty(&install).unwrap(),
    )
    .unwrap();
}

pub const INVALID_INSTALL_CASES: [&str; 6] = [
    "missing",
    "malformed",
    "root-hash",
    "local-hash",
    "version",
    "registry-only-old",
];

pub fn invalidate(root: &Path, case: &str) {
    let metadata = root.join("wally.install.json");
    match case {
        "missing" => fs::remove_file(metadata).unwrap(),
        "malformed" => fs::write(metadata, "not JSON").unwrap(),
        "root-hash" => {
            let path = root.join("wally.toml");
            let contents = fs::read_to_string(&path).unwrap();
            fs::write(path, format!("{contents}\n")).unwrap();
        }
        "local-hash" => {
            let path = root.join("Source/ForkedPackages/foo/wally.toml");
            let contents = fs::read_to_string(&path).unwrap();
            fs::write(path, format!("{contents}\n")).unwrap();
        }
        "version" => {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
            value["version"] = json!(999);
            fs::write(metadata, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        "registry-only-old" => {
            fs::remove_file(metadata).unwrap();
            fs::write(root.join("wally.toml"), "[package]\nname = \"scope/app\"\nversion = \"1.0.0\"\nrealm = \"shared\"\n[dependencies]\nRegistry = \"scope/registry@1.2.3\"\n").unwrap();
        }
        _ => panic!("Unknown invalid provenance fixture"),
    }
}

pub fn source_update(
    id: rbx_dom_weak::types::Ref,
    source: &str,
) -> librojo::web_api::InstanceUpdate {
    use rbx_dom_weak::{types::Variant, ustr, UstrMap};
    librojo::web_api::InstanceUpdate {
        id,
        changed_name: None,
        changed_class_name: None,
        changed_parent: None,
        changed_metadata: None,
        changed_properties: UstrMap::from_iter([(
            ustr("Source"),
            Some(Variant::String(source.into())),
        )]),
    }
}

/// Compare every user-owned byte, not just the file a write was meant to target.
pub fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let path = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (!path.starts_with(".rojo/")).then(|| (path, fs::read(entry.path()).unwrap()))
        })
        .collect()
}

#[test]
fn file_comparisons_use_portable_keys_and_exclude_recovery_state() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("Source/nested")).unwrap();
    fs::create_dir_all(root.join(".rojo/relocations/batch")).unwrap();
    fs::write(root.join("Source/nested/init.luau"), b"return 1\n").unwrap();
    fs::write(root.join(".rojo/relocations/batch/backup"), b"private").unwrap();

    assert_eq!(
        files(root),
        BTreeMap::from([("Source/nested/init.luau".into(), b"return 1\n".to_vec())])
    );
}
