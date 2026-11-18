use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context};
use memofs::{IoResultExt, Vfs};

use crate::project::Project;

const LFS_SUFFIX: &str = " filter=lfs diff=lfs merge=lfs -text";

/// An additive shared edit, independent of any one serialized instance.
#[derive(Clone)]
pub(super) struct AttributesEdit {
    pub expected: Option<Vec<u8>>,
    pub contents: Vec<u8>,
    rules: BTreeSet<String>,
}

impl AttributesEdit {
    pub fn new(expected: Option<Vec<u8>>, rule: String) -> anyhow::Result<Self> {
        if let Some(bytes) = &expected {
            std::str::from_utf8(bytes).context("LFS attributes must contain valid UTF-8")?;
        }
        let mut edit = Self {
            contents: Vec::new(),
            expected,
            rules: BTreeSet::from([rule]),
        };
        edit.render();
        Ok(edit)
    }

    pub fn merge(&mut self, other: Self) -> anyhow::Result<()> {
        ensure!(
            self.expected == other.expected,
            "LFS attributes plans have inconsistent original contents"
        );
        self.rules.extend(other.rules);
        self.render();
        Ok(())
    }

    fn render(&mut self) {
        let original = self.expected.as_deref().unwrap_or_default();
        let text = std::str::from_utf8(original).expect("validated attributes");
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        // Only the trailing LFS records are known not to have later overrides.
        let trailing_rules: BTreeSet<_> = text
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .take_while(|line| line.starts_with('"') && line.ends_with(LFS_SUFFIX))
            .collect();
        self.contents = original.to_vec();
        for rule in &self.rules {
            if trailing_rules.contains(rule.as_str()) {
                continue;
            }
            if !self.contents.is_empty() && !self.contents.ends_with(b"\n") {
                self.contents.extend_from_slice(newline.as_bytes());
            }
            self.contents.extend_from_slice(rule.as_bytes());
            self.contents.extend_from_slice(newline.as_bytes());
        }
    }
}

/// Quotes the Git attribute token, independently escaping wildmatch syntax.
fn exact_rule(relative: &Path) -> anyhow::Result<String> {
    let mut pattern = String::from("/");
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            anyhow::bail!("LFS paths must be normalized relative file paths");
        };
        if index > 0 {
            pattern.push('/');
        }
        let component = component
            .to_str()
            .context("LFS paths must contain valid UTF-8")?;
        for ch in component.chars() {
            if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
    }
    ensure!(pattern != "/", "LFS requires a file path");
    let mut quoted = String::from("\"");
    for ch in pattern.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            ch if ch.is_ascii_control() => quoted.push_str(&format!("\\{:03o}", ch as u32)),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted.push_str(LFS_SUFFIX);
    Ok(quoted)
}

pub(super) fn plan(
    project: &Project,
    vfs: &Vfs,
    target: &Path,
) -> anyhow::Result<(PathBuf, AttributesEdit)> {
    ensure!(
        !target
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "LFS target must be normalized"
    );
    target
        .to_str()
        .context("LFS paths must contain valid UTF-8")?;
    let base = project.folder_location();
    let mut lock = vfs.lock();
    // External sources need a rule within their own scope. Never create the future
    // model destination's parent, since relocation installs that directory itself.
    let directory = if target.starts_with(base) {
        let mut directory = target.parent().context("LFS target needs a parent")?;
        loop {
            if lock
                .metadata(directory.join(".gitattributes"))
                .with_not_found()?
                .is_some()
                || directory == base
            {
                break directory.to_path_buf();
            }
            directory = directory
                .parent()
                .context("LFS target is outside the project")?;
        }
    } else {
        let mut directory = target.parent().context("LFS target needs a parent")?;
        loop {
            if lock
                .metadata(directory)
                .with_not_found()?
                .is_some_and(|m| m.is_dir())
            {
                break directory.to_path_buf();
            }
            directory = directory
                .parent()
                .context("LFS target has no existing ancestor directory")?;
        }
    };
    let path = directory.join(".gitattributes");
    let globs = project
        .syncback_rules
        .as_ref()
        .map(|rules| rules.compile_globs())
        .transpose()?;
    ensure!(
        super::is_valid_path(&globs, base, &path),
        "Cannot write Terrain because LFS attributes {} are excluded by syncback ignorePaths",
        path.display()
    );
    let relative = target
        .strip_prefix(&directory)
        .context("LFS target is outside attributes scope")?;
    let rule = exact_rule(relative)?;
    let expected = lock
        .read(&path)
        .with_not_found()?
        .map(|bytes| (*bytes).clone());
    Ok((path, AttributesEdit::new(expected, rule)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use memofs::{InMemoryFs, VfsSnapshot};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn git_command() -> Command {
        let mut command = Command::new("git");
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_TEMPLATE_DIR",
        ] {
            command.env_remove(key);
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args(["-c", &format!("core.attributesFile={null}")]);
        command
    }

    fn project(ignore: &str) -> (Project, Vfs) {
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        backend
            .load_snapshot("/external", VfsSnapshot::empty_dir())
            .unwrap();
        backend.load_snapshot("/project/default.project.json", VfsSnapshot::file(format!(
            r#"{{"name":"Test","tree":{{"$className":"DataModel"}},"syncbackRules":{{"ignorePaths":{ignore}}}}}"#
        ))).unwrap();
        let vfs = Vfs::new(backend);
        let project =
            Project::load_exact(&vfs, Path::new("/project/default.project.json"), None).unwrap();
        (project, vfs)
    }

    #[test]
    fn preserves_original_bytes_and_merges_rules_deterministically() {
        let original = Some(b"# Keep this comment\r\n*.txt text\r\n# no final newline".to_vec());
        let a = exact_rule(Path::new("a.rbxm")).unwrap();
        let b = exact_rule(Path::new("b.rbxm")).unwrap();
        let mut forward = AttributesEdit::new(original.clone(), b.clone()).unwrap();
        forward
            .merge(AttributesEdit::new(original.clone(), a.clone()).unwrap())
            .unwrap();
        let mut backward = AttributesEdit::new(original.clone(), a).unwrap();
        backward
            .merge(AttributesEdit::new(original.clone(), b.clone()).unwrap())
            .unwrap();
        assert_eq!(forward.contents, backward.contents);
        assert!(forward.contents.starts_with(original.as_ref().unwrap()));
        assert!(forward.contents.ends_with(format!("{b}\r\n").as_bytes()));
        let repeated = AttributesEdit::new(Some(forward.contents.clone()), b).unwrap();
        assert_eq!(repeated.contents, forward.contents);
        assert!(forward
            .merge(AttributesEdit::new(None, "other".into()).unwrap())
            .is_err());
        assert!(AttributesEdit::new(Some(vec![255]), "rule".into()).is_err());
    }

    #[test]
    fn plans_external_sources_at_existing_ancestor_and_refuses_ignored_attributes() {
        let (project, vfs) = project("[]");
        let (path, edit) =
            plan(&project, &vfs, Path::new("/external/future/Terrain.rbxm")).unwrap();
        assert_eq!(path, Path::new("/external/.gitattributes"));
        assert_eq!(
            edit.contents,
            format!(
                "{}\n",
                exact_rule(Path::new("future/Terrain.rbxm")).unwrap()
            )
            .as_bytes()
        );
        assert!(vfs.read(path).is_err());
        assert!(vfs.metadata("/external/future").is_err());
        let (ignored_project, ignored_vfs) = self::project(r#"[".gitattributes"]"#);
        let error = plan(
            &ignored_project,
            &ignored_vfs,
            Path::new("/project/Terrain.rbxm"),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ignorePaths"));
    }

    #[test]
    fn later_overrides_are_repaired_without_repeated_growth() {
        let rule = exact_rule(Path::new("Terrain.rbxm")).unwrap();
        let original = format!("{rule}\n*.rbxm -filter -diff -merge text\n");
        let edit = AttributesEdit::new(Some(original.into_bytes()), rule.clone()).unwrap();
        assert!(edit.contents.ends_with(format!("{rule}\n").as_bytes()));
        let other = exact_rule(Path::new("Other.rbxm")).unwrap();
        let mut combined = edit.clone();
        combined
            .merge(AttributesEdit::new(edit.expected.clone(), other.clone()).unwrap())
            .unwrap();
        for desired in [rule, other] {
            assert_eq!(
                AttributesEdit::new(Some(combined.contents.clone()), desired)
                    .unwrap()
                    .contents,
                combined.contents
            );
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".gitattributes"), edit.contents).unwrap();
        assert!(git_command()
            .args(["init", "--quiet", "--template="])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        let output = git_command()
            .args([
                "check-attr",
                "filter",
                "diff",
                "merge",
                "text",
                "--",
                "Terrain.rbxm",
            ])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(),
            "Terrain.rbxm: filter: lfs\nTerrain.rbxm: diff: lfs\nTerrain.rbxm: merge: lfs\nTerrain.rbxm: text: unset\n");
    }

    #[test]
    fn nearest_attributes_scope_wins_over_project_rules() {
        let (project, vfs) = project("[]");
        vfs.create_dir_all("/project/src").unwrap();
        vfs.write("/project/.gitattributes", b"*.rbxm filter=lfs\n")
            .unwrap();
        vfs.write("/project/src/.gitattributes", b"*.rbxm -filter\n")
            .unwrap();
        let (path, edit) = plan(
            &project,
            &vfs,
            Path::new("/project/src/future/Terrain.rbxm"),
        )
        .unwrap();
        assert_eq!(path, Path::new("/project/src/.gitattributes"));
        assert!(vfs.metadata("/project/src/future").is_err());
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join(".gitattributes"), "*.rbxm -filter\n").unwrap();
        std::fs::write(directory.path().join("src/.gitattributes"), edit.contents).unwrap();
        assert!(git_command()
            .args(["init", "--quiet", "--template="])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        let output = git_command()
            .args(["check-attr", "filter", "--", "src/future/Terrain.rbxm"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"src/future/Terrain.rbxm: filter: lfs\n");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"bad\xff.rbxm"));
        assert!(exact_rule(path).unwrap_err().to_string().contains("UTF-8"));
    }

    #[test]
    fn git_lfs_clean_and_smudge_round_trip_when_available() {
        if !git_command()
            .args(["lfs", "version"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("Skipping the optional round-trip test because Git LFS is unavailable.");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        assert!(git_command()
            .args(["init", "--quiet", "--template="])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success());
        let run = |operation: &str, contents: &[u8]| {
            let mut command = git_command()
                .args(["lfs", operation, "Terrain.rbxm"])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env_remove("GIT_LFS_SKIP_SMUDGE")
                .current_dir(directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            command.stdin.take().unwrap().write_all(contents).unwrap();
            let output = command.wait_with_output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let model = b"<roblox!\0binary Terrain payload\xff";
        let pointer = run("clean", model);
        assert!(pointer.starts_with(b"version https://git-lfs.github.com/spec/v1\n"));
        assert_ne!(pointer, model);
        assert_eq!(run("smudge", &pointer), model);
    }

    #[test]
    fn git_recognizes_exact_escaped_paths_without_tracking_neighbors() {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            git_command()
                .args(args)
                .current_dir(directory.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "--quiet", "--template="]).status.success());
        let names = [
            "space name.rbxm",
            "[bracket].rbxm",
            "star*.rbxm",
            "question?.rbxm",
            "quote\".rbxm",
            "#hash.rbxm",
            "!bang.rbxm",
            "back\\slash.rbxm",
            "line\nfeed.rbxm",
            "folder/file.rbxm",
        ];
        let rules = names
            .iter()
            .map(|name| exact_rule(Path::new(name)).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(directory.path().join(".gitattributes"), rules).unwrap();
        let mut process = git_command()
            .args([
                "check-attr",
                "-z",
                "--stdin",
                "filter",
                "diff",
                "merge",
                "text",
            ])
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = process.stdin.take().unwrap();
        for name in names {
            input.write_all(name.as_bytes()).unwrap();
            input.write_all(&[0]).unwrap();
        }
        for name in [
            "starOTHER.rbxm",
            "b.rbxm",
            "questionX.rbxm",
            "other/file.rbxm",
        ] {
            input.write_all(name.as_bytes()).unwrap();
            input.write_all(&[0]).unwrap();
        }
        drop(input);
        let output = process.wait_with_output().unwrap();
        assert!(output.status.success());
        let values: Vec<_> = output.stdout.split(|b| *b == 0).collect();
        for (index, name) in names.iter().enumerate() {
            for (attribute, value) in [
                ("filter", "lfs"),
                ("diff", "lfs"),
                ("merge", "lfs"),
                ("text", "unset"),
            ] {
                let offset = index * 12
                    + match attribute {
                        "filter" => 0,
                        "diff" => 3,
                        "merge" => 6,
                        _ => 9,
                    };
                assert_eq!(values[offset], name.as_bytes());
                assert_eq!(values[offset + 1], attribute.as_bytes());
                assert_eq!(values[offset + 2], value.as_bytes(), "{name:?}");
            }
        }
        for triple in values[names.len() * 12..values.len() - 1].chunks_exact(3) {
            assert_eq!(triple[2], b"unspecified");
        }
    }
}
