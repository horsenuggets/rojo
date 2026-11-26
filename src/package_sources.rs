//! Wally install provenance is derived data, never an instruction to create a fork.
//! Ownership is validated before reading original sources and again before writes.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use memofs::{IoResultExt, Vfs};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Install {
    version: u32,
    manifest_hash: String,
    local_manifests: BTreeMap<String, String>,
    pub package_roots: Vec<PackageRoot>,
    pub generated_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageRoot {
    pub name: String,
    pub kind: Kind,
    pub realm: Realm,
    pub install_path: String,
    #[serde(
        default,
        deserialize_with = "present_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_path: Option<String>,
    #[serde(
        default,
        deserialize_with = "present_string",
        skip_serializing_if = "Option::is_none"
    )]
    package_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Local,
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Realm {
    Shared,
    Server,
    Dev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackageSources {
    pub root: PathBuf,
    pub install: Option<Install>,
    /// A missing or invalid generation still protects Wally's installation surfaces.
    pub problem: Option<String>,
    local_sources: BTreeSet<PathBuf>,
    deny_all_writes: bool,
}

fn present_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn relative(path: &str) -> anyhow::Result<&Path> {
    ensure!(
        !path.is_empty() && !path.contains('\\') && !path.contains(':') && !path.ends_with('/'),
        "Invalid project-relative provenance path {path:?}"
    );
    let result = Path::new(path);
    ensure!(
        result
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            && result
                .components()
                .map(|part| part.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/")
                == path,
        "Provenance path must be normalized and cannot escape the project {path:?}"
    );
    Ok(result)
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    // Match the compatible Windows spelling returned by the VFS canonicalizer.
    let simplified = dunce::simplified(&normalized);
    if simplified == normalized {
        normalized
    } else {
        simplified.to_path_buf()
    }
}

/// Checks the closest existing ancestor as well, so additions cannot traverse a link.
pub(crate) fn contained(vfs: &Vfs, root: &Path, path: &Path) -> anyhow::Result<()> {
    ensure!(
        path.starts_with(root),
        "Path escapes package project {}",
        path.display()
    );
    let canonical_root = vfs.canonicalize(root)?;
    for ancestor in path.ancestors() {
        if let Some(resolved) = vfs.canonicalize(ancestor).with_not_found()? {
            ensure!(
                resolved.starts_with(&canonical_root),
                "Package path escapes through a symlink {}",
                path.display()
            );
            break;
        }
    }
    Ok(())
}

fn manifest(vfs: &Vfs, path: &Path) -> anyhow::Result<(Vec<u8>, toml::Value)> {
    let bytes = vfs.read(path)?.to_vec();
    let value = toml::from_str(std::str::from_utf8(&bytes)?)?;
    Ok((bytes, value))
}

fn local_requirements(value: &toml::Value, include_dev: bool) -> anyhow::Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for section in ["dependencies", "server-dependencies", "dev-dependencies"] {
        if section == "dev-dependencies" && !include_dev {
            continue;
        }
        if let Some(table) = value.get(section).and_then(toml::Value::as_table) {
            for requirement in table.values() {
                if let Some(path) = requirement.as_str().and_then(|s| s.strip_prefix('^')) {
                    ensure!(
                        !path.contains('\\')
                            && !path.contains(':')
                            && !Path::new(path).is_absolute(),
                        "Local declaration must be project-relative"
                    );
                    let mut declared = PathBuf::new();
                    for part in Path::new(path).components() {
                        match part {
                            Component::Normal(part) => declared.push(part),
                            Component::CurDir => {}
                            Component::ParentDir => {
                                ensure!(declared.pop(), "Local declaration escapes the project")
                            }
                            _ => bail!("Invalid local declaration"),
                        }
                    }
                    ensure!(
                        !declared.as_os_str().is_empty(),
                        "A local package cannot claim the install project itself"
                    );
                    paths.insert(declared);
                }
            }
        }
    }
    Ok(paths)
}

impl PackageSources {
    pub fn load(vfs: &Vfs, root: &Path) -> anyhow::Result<Option<Self>> {
        if vfs
            .metadata(root.join("wally.toml"))
            .with_not_found()?
            .is_none()
        {
            ensure!(
                vfs.metadata(root.join("wally.install.json"))
                    .with_not_found()?
                    .is_none(),
                "wally.install.json has no authoritative wally.toml"
            );
            return Ok(None);
        }
        let mut policy = Self {
            root: root.to_path_buf(),
            install: None,
            problem: None,
            local_sources: BTreeSet::new(),
            deny_all_writes: false,
        };
        match policy.validate(vfs) {
            Ok(install) => policy.install = Some(install),
            Err(error) => {
                // Even without usable metadata, all reachable declared local
                // sources remain quarantined. Never guess ownership from names.
                policy.deny_all_writes = policy.quarantine_declared_sources(vfs).is_err();
                let message = format!("Invalid or missing wally.install.json. Run Wally install before package syncback. {error:#}");
                log::warn!("{message}");
                policy.problem = Some(message);
            }
        }
        Ok(Some(policy))
    }

    fn quarantine_declared_sources(&mut self, vfs: &Vfs) -> anyhow::Result<()> {
        contained(vfs, &self.root, &self.root.join("wally.toml"))?;
        let (_, root) = manifest(vfs, &self.root.join("wally.toml"))?;
        self.local_sources = local_requirements(&root, true)?;
        let mut pending = self.local_sources.clone();
        let mut visited = BTreeSet::new();
        while let Some(source) = pending.pop_first() {
            if !visited.insert(source.clone()) {
                continue;
            }
            let path = self.root.join(source).join("wally.toml");
            contained(vfs, &self.root, &path)?;
            let (_, value) = manifest(vfs, &path)?;
            for source in local_requirements(&value, false)? {
                self.local_sources.insert(source.clone());
                pending.insert(source);
            }
        }
        Ok(())
    }

    fn validate(&mut self, vfs: &Vfs) -> anyhow::Result<Install> {
        contained(vfs, &self.root, &self.root.join("wally.toml"))?;
        contained(vfs, &self.root, &self.root.join("wally.install.json"))?;
        let (bytes, root_manifest) = manifest(vfs, &self.root.join("wally.toml"))?;
        self.local_sources = local_requirements(&root_manifest, true)?;
        let install: Install =
            serde_json::from_slice(&vfs.read(self.root.join("wally.install.json"))?)?;
        ensure!(
            install.version == 1,
            "Unsupported Wally provenance version {}",
            install.version
        );
        ensure!(
            install.manifest_hash == sha256(&bytes),
            "Root wally.toml hash changed"
        );
        let mut pending = self.local_sources.clone();
        let mut visited = BTreeSet::new();
        while let Some(source) = pending.pop_first() {
            if !visited.insert(source.clone()) {
                continue;
            }
            contained(vfs, &self.root, &self.root.join(&source))?;
            let manifest_path = source.join("wally.toml");
            contained(vfs, &self.root, &self.root.join(&manifest_path))?;
            let (bytes, value) = manifest(vfs, &self.root.join(&manifest_path))?;
            let key = manifest_path
                .to_str()
                .context("Non-UTF8 manifest path")?
                .replace('\\', "/");
            ensure!(
                install.local_manifests.get(&key) == Some(&sha256(&bytes)),
                "Local manifest hash missing or changed for {key}"
            );
            for dependency in local_requirements(&value, false)? {
                self.local_sources.insert(dependency.clone());
                pending.insert(dependency);
            }
        }
        ensure!(
            install.local_manifests.len() == visited.len(),
            "Stale local manifest ownership"
        );
        let mut destinations = BTreeSet::new();
        let mut represented = BTreeSet::new();
        for package in &install.package_roots {
            let destination = relative(&package.install_path)?;
            let output = match package.realm {
                Realm::Shared => "Packages",
                Realm::Server => "ServerPackages",
                Realm::Dev => "DevPackages",
            };
            ensure!(
                destination.starts_with(output) && destination.components().count() > 1,
                "Package installation does not match its realm"
            );
            contained(vfs, &self.root, &self.root.join(destination))?;
            ensure!(
                destinations
                    .iter()
                    .all(|other: &PathBuf| !destination.starts_with(other)
                        && !other.starts_with(destination)),
                "Overlapping package installation ownership"
            );
            destinations.insert(destination.to_path_buf());
            ensure!(
                relative(&package.name)?.components().count() == 2,
                "Invalid scoped package identity"
            );
            match package.kind {
                Kind::Local => {
                    ensure!(
                        package.package_version.is_none(),
                        "Local package cannot have a registry version"
                    );
                    let source = relative(
                        package
                            .source_path
                            .as_deref()
                            .context("Local package requires source_path")?,
                    )?;
                    ensure!(
                        visited.contains(source),
                        "Package source is not declared by wally.toml"
                    );
                    let (_, value) = manifest(vfs, &self.root.join(source).join("wally.toml"))?;
                    ensure!(
                        value
                            .get("package")
                            .and_then(|p| p.get("name"))
                            .and_then(toml::Value::as_str)
                            == Some(package.name.as_str()),
                        "Local package identity differs from manifest"
                    );
                    ensure!(
                        represented.insert(source.to_path_buf()),
                        "A local source has multiple installed module identities"
                    );
                }
                Kind::Registry => {
                    ensure!(
                        package.source_path.is_none(),
                        "Registry package cannot claim a source path"
                    );
                    let version = package
                        .package_version
                        .as_deref()
                        .context("Registry package requires package_version")?;
                    semver::Version::parse(version).context("Invalid registry package version")?;
                }
            }
        }
        ensure!(
            represented == visited,
            "Declared local packages are missing from install provenance"
        );
        for source in &visited {
            ensure!(
                destinations
                    .iter()
                    .all(|dest| !source.starts_with(dest) && !dest.starts_with(source)),
                "Package source overlaps installation"
            );
        }
        let mut generated = BTreeSet::new();
        for path in &install.generated_paths {
            let path = relative(path)?;
            ensure!(
                ["Packages", "ServerPackages", "DevPackages"]
                    .iter()
                    .any(|output| path.starts_with(output))
                    && path.components().count() > 1,
                "Generated shim is outside Wally installation surfaces"
            );
            contained(vfs, &self.root, &self.root.join(path))?;
            ensure!(
                generated.insert(path.to_path_buf()),
                "Duplicate generated path"
            );
            ensure!(
                visited.iter().all(|source| !path.starts_with(source)),
                "Generated shim overlaps authoritative source"
            );
        }
        Ok(install)
    }

    pub fn is_local_source(&self, path: &Path) -> bool {
        self.local_source_owner(path).is_some()
    }

    pub fn local_source_owner(&self, path: &Path) -> Option<PathBuf> {
        let path = normalize(path);
        self.local_sources
            .iter()
            .map(|source| self.root.join(source))
            .filter(|source| path.starts_with(source))
            .max_by_key(|source| source.components().count())
    }

    pub fn original_path(&self, path: &Path) -> PathBuf {
        if let Some(install) = &self.install {
            for package in &install.package_roots {
                if let Some(source) = &package.source_path {
                    if let Ok(suffix) = path.strip_prefix(self.root.join(&package.install_path)) {
                        return self.root.join(source).join(suffix);
                    }
                }
            }
        }
        path.to_path_buf()
    }

    pub fn local_mount<'a>(&'a self, path: &Path) -> Option<&'a PackageRoot> {
        self.install.as_ref()?.package_roots.iter().find(|package| {
            package.kind == Kind::Local
                && normalize(path).starts_with(self.root.join(&package.install_path))
        })
    }

    /// Container directories can be traversed without granting ownership of their
    /// generated children. Only a concrete read-only node stops serialization.
    pub fn read_only_node(&self, vfs: &Vfs, path: &Path) -> Option<String> {
        let normalized = normalize(path);
        if let Some(install) = &self.install {
            let owned = install
                .package_roots
                .iter()
                .any(|package| normalized.starts_with(self.root.join(&package.install_path)))
                || install
                    .generated_paths
                    .iter()
                    .any(|generated| normalized == self.root.join(generated))
                || install
                    .local_manifests
                    .keys()
                    .any(|manifest| normalized == self.root.join(manifest))
                || ["wally.toml", "wally.install.json", "wally.lock"]
                    .iter()
                    .any(|name| normalized == self.root.join(name));
            if owned {
                return self.blocked(vfs, &normalized, false);
            }
            None
        } else {
            self.blocked(vfs, &normalized, false)
        }
    }

    /// None means application-owned. Some explains why a Studio write is blocked.
    /// Ancestor operations are checked too, preventing deletion or model absorption
    /// of a protected subtree through an application-owned parent.
    pub fn blocked(&self, vfs: &Vfs, path: &Path, subtree: bool) -> Option<String> {
        if self.deny_all_writes {
            return self.problem.clone();
        }
        let normalized = normalize(path);
        let path = normalized.as_path();
        let overlaps = |protected: &Path| {
            path.starts_with(protected) || (subtree && protected.starts_with(path))
        };
        if overlaps(&self.root.join("wally.toml"))
            || overlaps(&self.root.join("wally.install.json"))
            || overlaps(&self.root.join("wally.lock"))
        {
            return Some("Wally manifests and provenance are not Studio-owned".into());
        }
        if let Some(install) = &self.install {
            for package in &install.package_roots {
                // Installed local copies are never write destinations. Snapshot routing
                // already gives accepted local edits their original physical paths.
                if overlaps(&self.root.join(&package.install_path)) {
                    return Some(format!(
                        "Package {} installation is read-only",
                        package.name
                    ));
                }
            }
            for generated in &install.generated_paths {
                if overlaps(&self.root.join(generated)) {
                    return Some(format!("Generated Wally shim {generated} is read-only"));
                }
            }
            for path in install
                .package_roots
                .iter()
                .map(|package| package.install_path.as_str())
                .chain(install.generated_paths.iter().map(String::as_str))
            {
                if let Some(surface) = Path::new(path).components().next() {
                    if overlaps(&self.root.join(surface)) {
                        return Some(
                            "Wally installation surfaces are generated and read-only".into(),
                        );
                    }
                }
            }
            for manifest in install.local_manifests.keys() {
                if overlaps(&self.root.join(manifest)) {
                    return Some(format!(
                        "Local package manifest {manifest} is authoritative"
                    ));
                }
            }
        } else {
            // These are Wally's declared installation surfaces, not guessed package
            // identities from directory names. Application paths remain writable.
            for folder in ["Packages", "ServerPackages", "DevPackages"] {
                if overlaps(&self.root.join(folder)) {
                    return Some(self.problem.clone().unwrap());
                }
            }
            for source in &self.local_sources {
                if overlaps(&self.root.join(source)) {
                    return Some(self.problem.clone().unwrap());
                }
            }
        }
        if self.is_local_source(path) {
            if let Err(error) = contained(vfs, &self.root, path) {
                return Some(format!("{error:#}"));
            }
        }
        None
    }

    pub fn check_generation(&self, vfs: &Vfs) -> anyhow::Result<()> {
        let current = Self::load(vfs, &self.root)?;
        ensure!(
            current.as_ref() == Some(self),
            "Wally install ownership changed during sync. Restart Rojo after installing"
        );
        if let Some(problem) = &self.problem {
            bail!("{problem}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memofs::{InMemoryFs, VfsSnapshot};
    use serde_json::{json, Value};

    #[cfg(windows)]
    #[test]
    fn normalization_unifies_compatible_windows_verbatim_paths() {
        let plain = Path::new(r"C:\checkout with spaces\Source");
        for path in [
            plain,
            Path::new(r"\\?\C:\checkout with spaces\Source"),
            Path::new(r"\\?\C:\checkout with spaces\Packages\..\Source"),
        ] {
            assert_eq!(normalize(path), plain);
            assert_eq!(normalize(&normalize(path)), plain);
        }
        let reserved = Path::new(r"\\?\C:\checkout\NUL");
        assert_eq!(normalize(reserved), reserved);
    }

    fn fixture(change: impl FnOnce(&mut Value)) -> Vfs {
        let root_manifest = "[dependencies]\nFoo = \"^Source/Foo\"\n";
        let local_manifest = "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n";
        let mut install = json!({
            "version":1, "manifest_hash":sha256(root_manifest.as_bytes()),
            "local_manifests":{"Source/Foo/wally.toml":sha256(local_manifest.as_bytes())},
            "package_roots":[{"name":"scope/foo", "kind":"local", "realm":"shared", "install_path":"Packages/_Index/foo/foo", "source_path":"Source/Foo"}],
            "generated_paths":["Packages/Foo.luau"]
        });
        change(&mut install);
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                ("wally.toml", VfsSnapshot::file(root_manifest)),
                (
                    "wally.install.json",
                    VfsSnapshot::file(serde_json::to_vec(&install).unwrap()),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([(
                        "Foo",
                        VfsSnapshot::dir([
                            ("wally.toml", VfsSnapshot::file(local_manifest)),
                            ("init.luau", VfsSnapshot::file("return 1")),
                        ]),
                    )]),
                ),
                ("Packages", VfsSnapshot::empty_dir()),
            ]),
        )
        .unwrap();
        Vfs::new(fs)
    }

    #[test]
    fn exact_hashes_allow_source_edits_but_not_manifest_edits() {
        let vfs = fixture(|_| {});
        let root = Path::new("/project");
        let policy = PackageSources::load(&vfs, root).unwrap().unwrap();
        assert!(policy.problem.is_none());
        vfs.write(root.join("Source/Foo/init.luau"), "return 2")
            .unwrap();
        policy.check_generation(&vfs).unwrap();
        vfs.write(
            root.join("Source/Foo/wally.toml"),
            "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n\n",
        )
        .unwrap();
        assert!(policy.check_generation(&vfs).is_err());
    }

    #[test]
    fn invalid_generations_fail_closed_without_claiming_application_paths() {
        let mutations: &[fn(&mut Value)] = &[
            |v| v["version"] = json!(2),
            |v| v["manifest_hash"] = json!("0".repeat(64)),
            |v| v["local_manifests"] = json!({}),
            |v| v["package_roots"][0]["install_path"] = json!("../outside"),
            |v| v["package_roots"][0]["source_path"] = json!("Source/./Foo"),
            |v| v["package_roots"][0]["source_path"] = json!("Source\\Foo"),
            |v| v["package_roots"][0]["source_path"] = json!("/outside"),
            |v| v["package_roots"][0]["name"] = json!("scope/other"),
            |v| v["package_roots"][0]["kind"] = json!("unknown"),
            |v| v["package_roots"][0]["package_version"] = Value::Null,
            |v| v["generated_paths"] = json!(["Source/Foo/init.luau"]),
            |v| v["package_roots"] = json!([]),
        ];
        for mutate in mutations {
            let vfs = fixture(mutate);
            let root = Path::new("/project");
            let policy = PackageSources::load(&vfs, root).unwrap().unwrap();
            assert!(policy.problem.is_some(), "{policy:?}");
            assert!(policy
                .blocked(&vfs, &root.join("Packages/New.luau"), false)
                .is_some());
            assert!(policy
                .blocked(&vfs, &root.join("Source/Foo/New.luau"), false)
                .is_some());
            assert!(policy
                .blocked(&vfs, &root.join("Source/App.luau"), false)
                .is_none());
        }
    }

    #[test]
    fn missing_and_malformed_metadata_do_not_disable_ordinary_filesystem_sync() {
        for contents in [None, Some("not JSON")] {
            let vfs = fixture(|_| {});
            let path = Path::new("/project/wally.install.json");
            if let Some(contents) = contents {
                vfs.write(path, contents).unwrap();
            } else {
                vfs.remove_file(path).unwrap();
            }
            let policy = PackageSources::load(&vfs, Path::new("/project"))
                .unwrap()
                .unwrap();
            assert!(policy.install.is_none());
            assert!(policy
                .blocked(&vfs, Path::new("/project/Source/App.luau"), false)
                .is_none());
            assert!(policy
                .blocked(&vfs, Path::new("/project/Packages/Untyped.luau"), false)
                .is_some());
        }
    }

    #[test]
    fn absent_metadata_quarantines_transitive_local_sources_too() {
        let vfs = fixture(|_| {});
        vfs.remove_file("/project/wally.install.json").unwrap();
        vfs.write("/project/Source/Foo/wally.toml", "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n[dependencies]\nBar = \"^Source/Bar\"\n").unwrap();
        vfs.create_dir_all("/project/Source/Bar").unwrap();
        vfs.write(
            "/project/Source/Bar/wally.toml",
            "[package]\nname = \"scope/bar\"\nrealm = \"shared\"\n",
        )
        .unwrap();
        let policy = PackageSources::load(&vfs, Path::new("/project"))
            .unwrap()
            .unwrap();
        assert!(policy
            .blocked(&vfs, Path::new("/project/Source/Bar/New.luau"), false)
            .is_some());
        assert!(policy
            .blocked(&vfs, Path::new("/project/Source/App.luau"), false)
            .is_none());
        assert!(policy
            .blocked(&vfs, Path::new("/explicit-external/App.luau"), false)
            .is_none());
    }

    #[test]
    fn unreadable_authoritative_declarations_cannot_grant_write_ownership() {
        let vfs = fixture(|_| {});
        vfs.write("/project/wally.toml", "invalid [[").unwrap();
        let policy = PackageSources::load(&vfs, Path::new("/project"))
            .unwrap()
            .unwrap();
        assert!(policy.deny_all_writes);
        assert!(policy
            .blocked(&vfs, Path::new("/project/Source/App.luau"), false)
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn containment_rejects_existing_and_future_symlink_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("Linked")).unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        assert!(contained(&vfs, &root, &root.join("Linked")).is_err());
        assert!(contained(&vfs, &root, &root.join("Linked/Future.luau")).is_err());
        assert!(contained(&vfs, &root, &root.join("Future/Source.luau")).is_ok());
    }

    #[test]
    fn ancestors_cannot_remove_packages_or_manifests() {
        let vfs = fixture(|_| {});
        let policy = PackageSources::load(&vfs, Path::new("/project"))
            .unwrap()
            .unwrap();
        assert!(policy
            .blocked(&vfs, Path::new("/project/Packages"), true)
            .is_some());
        assert!(policy
            .blocked(&vfs, Path::new("/project/Source/Foo"), true)
            .is_some());
        assert!(policy
            .blocked(&vfs, Path::new("/project/Source/Foo/init.luau"), false)
            .is_none());
    }
}
