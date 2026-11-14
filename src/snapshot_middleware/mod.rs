//! Defines the semantics that Rojo uses to turn entries on the filesystem into
//! Roblox instances using the instance snapshot subsystem.
//!
//! These modules define how files turn into instances.

#![allow(dead_code)]

mod csv;
mod dir;
mod json;
mod json_model;
mod lua;
mod luaurc;
mod meta_file;
mod project;
mod rbxm;
mod rbxmx;
mod toml;
mod txt;
mod util;
mod yaml;

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context;
use memofs::{DirEntry, IoResultExt, Vfs};
use serde::{Deserialize, Serialize};

use crate::{
    glob::Glob,
    project::DEFAULT_PROJECT_NAMES,
    syncback::{SyncbackReturn, SyncbackSnapshot},
};
use crate::{
    snapshot::{InstanceContext, InstanceSnapshot, SyncRule},
    syncback::validate_file_name,
};

use self::{
    csv::{snapshot_csv, snapshot_csv_init, syncback_csv, syncback_csv_init},
    dir::{snapshot_dir, syncback_dir},
    json::snapshot_json,
    json_model::{snapshot_json_model, syncback_json_model},
    lua::{snapshot_lua, snapshot_lua_init, syncback_lua, syncback_lua_init},
    project::{snapshot_project, syncback_project},
    rbxm::{snapshot_rbxm, syncback_rbxm},
    rbxmx::{snapshot_rbxmx, syncback_rbxmx},
    toml::snapshot_toml,
    txt::{snapshot_txt, syncback_txt},
    yaml::snapshot_yaml,
};

pub use self::{
    lua::ScriptType, project::snapshot_project_node, util::emit_legacy_scripts_default,
    util::PathExt,
};

/// Snapshots `path` without the children that are stored in their own files.
/// Directories keep children written into their metadata. The result must be
/// compared with `compute_shallow_patch_set`, which doesn't treat the children
/// left out here as removed.
pub fn snapshot_shallow_from_vfs(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let mut shallow = context.clone();
    shallow.shallow = true;
    let mut snapshot = snapshot_from_vfs(&shallow, vfs, path)?;
    // The flag only steers this read. Leaving it set would differ from the
    // tree's metadata and could later make a real snapshot skip children.
    if let Some(snapshot) = &mut snapshot {
        clear_shallow(snapshot);
    }
    Ok(snapshot)
}

fn clear_shallow(snapshot: &mut InstanceSnapshot) {
    snapshot.metadata.context.shallow = false;
    for child in &mut snapshot.children {
        clear_shallow(child);
    }
}

/// Returns an `InstanceSnapshot` for the provided path.
/// This will inspect the path and find the appropriate middleware for it,
/// taking user-written rules into account. Then, it will attempt to convert
/// the path into an InstanceSnapshot using that middleware.
#[profiling::function]
pub fn snapshot_from_vfs(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let meta = match vfs.metadata(path).with_not_found()? {
        Some(meta) => meta,
        None => return Ok(None),
    };

    if meta.is_dir() {
        // Watched discovery must not recursively watch an entire project root.
        // Only non-watching builds can discard enumeration errors and fall back
        // to direct probes for searchable but non-listable directories.
        let entries = if vfs.is_watch_enabled() {
            None
        } else {
            vfs.read_dir(path)
                .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
                .ok()
        };
        let (middleware, dir_name, init_path) = get_dir_middleware(vfs, path, entries.as_deref())?;
        // TODO: Support user defined init paths
        // If and when we do, make sure to go support it in
        // `Project::set_file_name`, as right now it special-cases
        // `default.project.json` as an `init` path.
        middleware.snapshot_with_entries(context, vfs, &init_path, dir_name, entries)
    } else {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("file name of {} is invalid", path.display()))?;

        // TODO: Is this even necessary anymore?
        match file_name {
            "init.server.luau" | "init.server.lua" | "init.client.luau" | "init.client.lua"
            | "init.legacy.luau" | "init.legacy.lua" | "init.local.luau" | "init.local.lua"
            | "init.plugin.luau" | "init.plugin.lua" | "init.luau" | "init.lua" | "init.csv" => {
                return Ok(None)
            }
            _ => {}
        }

        snapshot_from_path(context, vfs, path)
    }
}

/// The `init` files that turn a directory into something other than a Folder,
/// in priority order. For compatibility, that order should be left unchanged.
pub(crate) fn dir_init_files() -> &'static [(Middleware, &'static str)] {
    static INIT_PATHS: OnceLock<Vec<(Middleware, &str)>> = OnceLock::new();
    INIT_PATHS.get_or_init(|| {
        vec![
            (Middleware::ModuleScriptDir, "init.luau"),
            (Middleware::ModuleScriptDir, "init.lua"),
            (Middleware::ServerScriptDir, "init.server.luau"),
            (Middleware::ServerScriptDir, "init.server.lua"),
            (Middleware::ClientScriptDir, "init.client.luau"),
            (Middleware::ClientScriptDir, "init.client.lua"),
            (Middleware::LegacyServerScriptDir, "init.legacy.luau"),
            (Middleware::LegacyServerScriptDir, "init.legacy.lua"),
            (Middleware::LegacyClientScriptDir, "init.local.luau"),
            (Middleware::LegacyClientScriptDir, "init.local.lua"),
            (Middleware::PluginScriptDir, "init.plugin.luau"),
            (Middleware::PluginScriptDir, "init.plugin.lua"),
            (Middleware::CsvDir, "init.csv"),
        ]
    })
}

/// Gets the appropriate middleware for a directory by checking for `init`
/// files. This uses an intrinsic priority list and for compatibility,
/// that order should be left unchanged.
///
/// Returns the middleware, the name of the directory, and the path to
/// the init location.
fn get_dir_middleware<'path>(
    vfs: &Vfs,
    dir_path: &'path Path,
    entries: Option<&[DirEntry]>,
) -> anyhow::Result<(Middleware, &'path str, PathBuf)> {
    let dir_name = dir_path
        .file_name()
        .expect("Could not extract directory name")
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("File name was not valid UTF-8: {}", dir_path.display()))?;

    let order = dir_init_files();

    // Names only narrow the candidates. Probe the original paths to preserve the
    // filesystem's case sensitivity and follow symlinks before checking file type.
    let has_candidate = |name: &str| {
        entries.is_none_or(|entries| {
            entries.iter().any(|entry| {
                entry
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|entry_name| entry_name.eq_ignore_ascii_case(name))
            })
        })
    };

    for default_project_name in DEFAULT_PROJECT_NAMES {
        if !has_candidate(default_project_name) {
            continue;
        }
        let project_path = dir_path.join(default_project_name);
        if vfs
            .metadata(&project_path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_file())
        {
            return Ok((Middleware::Project, dir_name, project_path));
        }
    }

    for (middleware, name) in order {
        if !has_candidate(name) {
            continue;
        }
        let test_path = dir_path.join(name);
        if vfs
            .metadata(&test_path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_file())
        {
            return Ok((*middleware, dir_name, test_path));
        }
    }

    Ok((Middleware::Dir, dir_name, dir_path.to_path_buf()))
}

/// Gets a snapshot for a path given an InstanceContext and Vfs, taking
/// user specified sync rules into account.
fn snapshot_from_path(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    if let Some(rule) = context.get_user_sync_rule(path) {
        return rule
            .middleware
            .snapshot(context, vfs, path, rule.file_name_for_path(path)?);
    } else {
        for rule in default_sync_rules() {
            if rule.matches(path) {
                return rule.middleware.snapshot(
                    context,
                    vfs,
                    path,
                    rule.file_name_for_path(path)?,
                );
            }
        }
    }
    Ok(None)
}

/// Represents a possible 'transformer' used by Rojo to turn a file system
/// item into a Roblox Instance. Missing from this list is metadata.
/// This is deliberate, as metadata is not a snapshot middleware.
///
/// Directories cannot be used for sync rules so they're ignored by Serde.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Middleware {
    Csv,
    JsonModel,
    Json,
    ServerScript,
    ClientScript,
    ModuleScript,
    PluginScript,
    LegacyClientScript,
    LegacyServerScript,
    RunContextServerScript,
    RunContextClientScript,
    Project,
    Rbxm,
    Rbxmx,
    Toml,
    Text,
    Yaml,
    Ignore,

    #[serde(skip_deserializing)]
    Dir,
    #[serde(skip_deserializing)]
    ServerScriptDir,
    #[serde(skip_deserializing)]
    ClientScriptDir,
    #[serde(skip_deserializing)]
    LegacyServerScriptDir,
    #[serde(skip_deserializing)]
    LegacyClientScriptDir,
    #[serde(skip_deserializing)]
    PluginScriptDir,
    #[serde(skip_deserializing)]
    ModuleScriptDir,
    #[serde(skip_deserializing)]
    CsvDir,
}

impl Middleware {
    /// Creates a snapshot for the given path from the Middleware with
    /// the provided name.
    fn snapshot(
        &self,
        context: &InstanceContext,
        vfs: &Vfs,
        path: &Path,
        name: &str,
    ) -> anyhow::Result<Option<InstanceSnapshot>> {
        self.snapshot_with_entries(context, vfs, path, name, None)
    }

    fn snapshot_with_entries(
        &self,
        context: &InstanceContext,
        vfs: &Vfs,
        path: &Path,
        name: &str,
        entries: Option<Vec<DirEntry>>,
    ) -> anyhow::Result<Option<InstanceSnapshot>> {
        let mut output = match self {
            Self::Csv => snapshot_csv(context, vfs, path, name),
            Self::JsonModel => snapshot_json_model(context, vfs, path, name),
            Self::Json => snapshot_json(context, vfs, path, name),
            Self::ServerScript => snapshot_lua(context, vfs, path, name, ScriptType::Server),
            Self::ClientScript => snapshot_lua(context, vfs, path, name, ScriptType::Client),
            Self::ModuleScript => snapshot_lua(context, vfs, path, name, ScriptType::Module),
            Self::PluginScript => snapshot_lua(context, vfs, path, name, ScriptType::Plugin),
            Self::LegacyClientScript => {
                snapshot_lua(context, vfs, path, name, ScriptType::LegacyClient)
            }
            Self::LegacyServerScript => {
                snapshot_lua(context, vfs, path, name, ScriptType::LegacyServer)
            }
            Self::RunContextClientScript => {
                snapshot_lua(context, vfs, path, name, ScriptType::RunContextClient)
            }
            Self::RunContextServerScript => {
                snapshot_lua(context, vfs, path, name, ScriptType::RunContextServer)
            }
            Self::Project => snapshot_project(context, vfs, path, name),
            Self::Rbxm => snapshot_rbxm(context, vfs, path, name),
            Self::Rbxmx => snapshot_rbxmx(context, vfs, path, name),
            Self::Toml => snapshot_toml(context, vfs, path, name),
            Self::Text => snapshot_txt(context, vfs, path, name),
            Self::Yaml => snapshot_yaml(context, vfs, path, name),
            Self::Ignore => Ok(None),

            Self::Dir => snapshot_dir(context, vfs, path, name, entries),
            Self::ServerScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::Server, entries)
            }
            Self::ClientScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::Client, entries)
            }
            Self::PluginScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::Plugin, entries)
            }
            Self::ModuleScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::Module, entries)
            }
            Self::LegacyServerScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::LegacyServer, entries)
            }
            Self::LegacyClientScriptDir => {
                snapshot_lua_init(context, vfs, path, name, ScriptType::LegacyClient, entries)
            }
            Self::CsvDir => snapshot_csv_init(context, vfs, path, name, entries),
        };
        if let Ok(Some(ref mut snapshot)) = output {
            snapshot.metadata.middleware = Some(*self);
        }
        output
    }

    /// Runs the syncback mechanism for the provided middleware given a
    /// SyncbackSnapshot.
    pub fn syncback<'sync>(
        &self,
        snapshot: &SyncbackSnapshot<'sync>,
    ) -> anyhow::Result<SyncbackReturn<'sync>> {
        let file_name = snapshot.path.file_name().and_then(|s| s.to_str());
        if let Some(file_name) = file_name {
            validate_file_name(file_name).with_context(|| {
                format!("cannot create a file or directory with name {file_name}")
            })?;
        }
        match self {
            Middleware::Csv => syncback_csv(snapshot),
            Middleware::JsonModel => syncback_json_model(snapshot),
            Middleware::Json => anyhow::bail!("cannot syncback Json middleware"),
            // Projects are only generated from files that already exist on the
            // file system, so we don't need to pass a file name.
            Middleware::Project => syncback_project(snapshot),
            Middleware::ServerScript => syncback_lua(snapshot),
            Middleware::ClientScript => syncback_lua(snapshot),
            Middleware::ModuleScript => syncback_lua(snapshot),
            Middleware::Rbxm => syncback_rbxm(snapshot),
            Middleware::Rbxmx => syncback_rbxmx(snapshot),
            Middleware::Toml => anyhow::bail!("cannot syncback Toml middleware"),
            Middleware::Text => syncback_txt(snapshot),
            Middleware::Yaml => anyhow::bail!("cannot syncback Yaml middleware"),
            Middleware::Ignore => anyhow::bail!("cannot syncback Ignore middleware"),
            Middleware::Dir => syncback_dir(snapshot),
            Middleware::ServerScriptDir => syncback_lua_init(ScriptType::Server, snapshot),
            Middleware::ClientScriptDir => syncback_lua_init(ScriptType::Client, snapshot),
            Middleware::PluginScriptDir => syncback_lua_init(ScriptType::Plugin, snapshot),
            Middleware::ModuleScriptDir => syncback_lua_init(ScriptType::Module, snapshot),
            Middleware::LegacyServerScriptDir => {
                syncback_lua_init(ScriptType::LegacyServer, snapshot)
            }
            Middleware::LegacyClientScriptDir => {
                syncback_lua_init(ScriptType::LegacyClient, snapshot)
            }
            Middleware::CsvDir => syncback_csv_init(snapshot),

            Middleware::PluginScript
            | Middleware::LegacyServerScript
            | Middleware::LegacyClientScript
            | Middleware::RunContextServerScript
            | Middleware::RunContextClientScript => {
                anyhow::bail!("syncback is not implemented for {self:?} yet")
            }
        }
    }

    /// Returns whether this particular middleware would become a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(
            self,
            Middleware::Dir
                | Middleware::ServerScriptDir
                | Middleware::ClientScriptDir
                | Middleware::PluginScriptDir
                | Middleware::ModuleScriptDir
                | Middleware::LegacyServerScriptDir
                | Middleware::LegacyClientScriptDir
                | Middleware::CsvDir
        )
    }

    /// Returns whether this particular middleware sets its own properties.
    /// This applies to things like `JsonModel` and `Project`, since they
    /// set properties without needing a meta.json file.
    ///
    /// It does not cover middleware like `ServerScript` or `Csv` because they
    /// need a meta.json file to set properties that aren't their designated
    /// 'special' properties.
    #[inline]
    pub fn handles_own_properties(&self) -> bool {
        matches!(
            self,
            Middleware::JsonModel | Middleware::Project | Middleware::Rbxm | Middleware::Rbxmx
        )
    }

    /// Attempts to return a middleware that should be used for the given path.
    ///
    /// Returns `Err` only if the Vfs cannot read information about the path.
    pub fn middleware_for_path(
        vfs: &Vfs,
        sync_rules: &[SyncRule],
        path: &Path,
    ) -> anyhow::Result<Option<Self>> {
        let meta = match vfs.metadata(path).with_not_found()? {
            Some(meta) => meta,
            None => return Ok(None),
        };

        if meta.is_dir() {
            let (middleware, _, _) = get_dir_middleware(vfs, path, None)?;
            Ok(Some(middleware))
        } else {
            for rule in sync_rules.iter().chain(default_sync_rules()) {
                if rule.matches(path) {
                    return Ok(Some(rule.middleware));
                }
            }
            Ok(None)
        }
    }
}

/// A helper for easily defining a SyncRule. Arguments are passed literally
/// to this macro in the order `include`, `middleware`, `suffix`,
/// and `exclude`. Both `suffix` and `exclude` are optional.
///
/// All arguments except `middleware` are expected to be strings.
/// The `middleware` parameter is expected to be a variant of `Middleware`,
/// not including the enum name itself.
macro_rules! sync_rule {
    ($pattern:expr, $middleware:ident) => {
        SyncRule {
            middleware: Middleware::$middleware,
            include: Glob::new($pattern).unwrap(),
            exclude: None,
            suffix: None,
            base_path: PathBuf::new(),
        }
    };
    ($pattern:expr, $middleware:ident, $suffix:expr) => {
        SyncRule {
            middleware: Middleware::$middleware,
            include: Glob::new($pattern).unwrap(),
            exclude: None,
            suffix: Some($suffix.into()),
            base_path: PathBuf::new(),
        }
    };
    ($pattern:expr, $middleware:ident, $suffix:expr, $exclude:expr) => {
        SyncRule {
            middleware: Middleware::$middleware,
            include: Glob::new($pattern).unwrap(),
            exclude: Some(Glob::new($exclude).unwrap()),
            suffix: Some($suffix.into()),
            base_path: PathBuf::new(),
        }
    };
}

/// Defines the 'default' syncing rules that Rojo uses.
/// These do not broadly overlap, but the order matters for some in the case of
/// e.g. JSON models.
pub fn default_sync_rules() -> &'static [SyncRule] {
    static DEFAULT_SYNC_RULES: OnceLock<Vec<SyncRule>> = OnceLock::new();

    DEFAULT_SYNC_RULES.get_or_init(|| {
        vec![
            sync_rule!("*.server.lua", ServerScript, ".server.lua"),
            sync_rule!("*.server.luau", ServerScript, ".server.luau"),
            sync_rule!("*.client.lua", ClientScript, ".client.lua"),
            sync_rule!("*.client.luau", ClientScript, ".client.luau"),
            sync_rule!("*.plugin.lua", PluginScript, ".plugin.lua"),
            sync_rule!("*.plugin.luau", PluginScript, ".plugin.luau"),
            sync_rule!("*.{lua,luau}", ModuleScript),
            sync_rule!("*.project.json", Project, ".project.json"),
            sync_rule!("*.project.jsonc", Project, ".project.jsonc"),
            sync_rule!("*.model.json", JsonModel, ".model.json"),
            sync_rule!("*.model.jsonc", JsonModel, ".model.jsonc"),
            sync_rule!("*.json", Json, ".json", "*.meta.json"),
            sync_rule!("*.jsonc", Json, ".jsonc", "*.meta.jsonc"),
            sync_rule!("*.toml", Toml),
            sync_rule!("*.csv", Csv),
            sync_rule!("*.txt", Text),
            sync_rule!("*.rbxmx", Rbxmx),
            sync_rule!("*.rbxm", Rbxm),
            sync_rule!("*.{yml,yaml}", Yaml),
        ]
    })
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};
    use std::path::Path;

    fn middleware_for_dir(entries: Vec<(&str, VfsSnapshot)>) -> Middleware {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/root", VfsSnapshot::dir(entries))
            .unwrap();

        let vfs = Vfs::new(imfs);
        let path = Path::new("/root");
        let entries = vfs
            .read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        let (middleware, _, _) = get_dir_middleware(&vfs, path, Some(&entries)).unwrap();
        middleware
    }

    #[test]
    fn legacy_server_init_selects_legacy_server_dir() {
        let middleware = middleware_for_dir(vec![(
            "init.legacy.luau",
            VfsSnapshot::file("print('server')"),
        )]);

        assert_eq!(middleware, Middleware::LegacyServerScriptDir);
    }

    #[test]
    fn alternate_init_files_do_not_become_children() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.legacy.luau", VfsSnapshot::file("print('luau')")),
                ("init.legacy.lua", VfsSnapshot::file("print('lua')")),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let instance_snapshot =
            snapshot_from_vfs(&InstanceContext::new(), &vfs, Path::new("/root"))
                .unwrap()
                .unwrap();

        assert_eq!(instance_snapshot.class_name, "Script");
        assert!(instance_snapshot.children.is_empty());
    }

    #[test]
    fn directories_named_like_initializers_are_not_initializers() {
        let middleware = middleware_for_dir(vec![
            ("init.luau", VfsSnapshot::empty_dir()),
            ("default.project.json", VfsSnapshot::empty_dir()),
        ]);

        assert_eq!(middleware, Middleware::Dir);
    }

    #[test]
    fn initializer_priority_is_preserved() {
        let order = [
            (Middleware::Project, "default.project.json"),
            (Middleware::Project, "default.project.jsonc"),
            (Middleware::ModuleScriptDir, "init.luau"),
            (Middleware::ModuleScriptDir, "init.lua"),
            (Middleware::ServerScriptDir, "init.server.luau"),
            (Middleware::ServerScriptDir, "init.server.lua"),
            (Middleware::ClientScriptDir, "init.client.luau"),
            (Middleware::ClientScriptDir, "init.client.lua"),
            (Middleware::LegacyServerScriptDir, "init.legacy.luau"),
            (Middleware::LegacyServerScriptDir, "init.legacy.lua"),
            (Middleware::LegacyClientScriptDir, "init.local.luau"),
            (Middleware::LegacyClientScriptDir, "init.local.lua"),
            (Middleware::PluginScriptDir, "init.plugin.luau"),
            (Middleware::PluginScriptDir, "init.plugin.lua"),
            (Middleware::CsvDir, "init.csv"),
        ];
        let path = Path::new("/root");

        for (index, &(expected, name)) in order.iter().enumerate() {
            let mut imfs = InMemoryFs::new();
            imfs.load_snapshot(
                path,
                VfsSnapshot::dir(
                    order[index..]
                        .iter()
                        .rev()
                        .map(|&(_, name)| (name, VfsSnapshot::file(""))),
                ),
            )
            .unwrap();
            let vfs = Vfs::new(imfs);
            let mut entries = vfs
                .read_dir(path)
                .unwrap()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap();
            entries.reverse();
            let (middleware, dir_name, init_path) =
                get_dir_middleware(&vfs, path, Some(&entries)).unwrap();

            assert_eq!(middleware, expected);
            assert_eq!(dir_name, "root");
            assert_eq!(init_path, path.join(name));
        }
    }

    #[test]
    fn initializer_directories_do_not_hide_lower_priority_files() {
        let middleware = middleware_for_dir(vec![
            ("default.project.json", VfsSnapshot::empty_dir()),
            ("init.luau", VfsSnapshot::empty_dir()),
            ("init.lua", VfsSnapshot::empty_dir()),
            ("init.server.luau", VfsSnapshot::file("")),
        ]);

        assert_eq!(middleware, Middleware::ServerScriptDir);
    }

    #[test]
    fn initializer_spelling_remains_case_sensitive() {
        for name in ["INIT.LUAU", "Init.lua", "DEFAULT.PROJECT.JSON", "INIT.CSV"] {
            assert_eq!(
                middleware_for_dir(vec![(name, VfsSnapshot::file(""))]),
                Middleware::Dir,
            );
        }

        assert_eq!(
            middleware_for_dir(vec![
                ("INIT.LUAU", VfsSnapshot::file("")),
                ("init.server.lua", VfsSnapshot::file("")),
            ]),
            Middleware::ServerScriptDir,
        );
    }

    #[test]
    fn initializer_lookup_respects_native_filesystem_case_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        std::fs::write(path.join("INIT.LUAU"), "return {}").unwrap();
        let expected = if path.join("init.luau").is_file() {
            Middleware::ModuleScriptDir
        } else {
            Middleware::Dir
        };
        let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
        let entries = vfs
            .read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        let (middleware, _, init_path) = get_dir_middleware(&vfs, path, Some(&entries)).unwrap();

        assert_eq!(middleware, expected);
        if expected == Middleware::ModuleScriptDir {
            assert_eq!(init_path, path.join("init.luau"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn default_project_in_non_listable_directory_remains_readable() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("root");
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("default.project.json"),
            r#"{ "name": "PermissionTest", "tree": { "$className": "Folder" } }"#,
        )
        .unwrap();
        let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o111)).unwrap();
        let listing_denied = vfs.read_dir(&directory).is_err();
        let snapshot = snapshot_from_vfs(&InstanceContext::new(), &vfs, &directory);
        let middleware = Middleware::middleware_for_path(&vfs, &[], &directory);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        if listing_denied {
            assert_eq!(snapshot.unwrap().unwrap().class_name, "Folder");
            assert_eq!(middleware.unwrap(), Some(Middleware::Project));
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_cycles_preserve_ordered_errors_and_allow_sibling_reuse() {
        use std::{fs, os::unix::fs::symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.rbxm"), "invalid").unwrap();
        for index in 0..6 {
            fs::write(root.join(format!("b{index}.txt")), "text").unwrap();
        }
        symlink(&root, root.join("cycle1")).unwrap();
        symlink(&root, root.join("cycle2")).unwrap();
        for workers in [1, 4] {
            let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            let error = pool
                .install(|| snapshot_from_vfs(&InstanceContext::new(), &vfs, &root))
                .unwrap_err();
            assert!(format!("{error:#}").contains("a.rbxm"));
        }
        fs::remove_file(root.join("a.rbxm")).unwrap();
        let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let error = snapshot_from_vfs(&InstanceContext::new(), &vfs, &root).unwrap_err();
        assert!(format!("{error:#}").contains("Directory dependency cycle"));
        fs::remove_file(root.join("cycle1")).unwrap();
        fs::remove_file(root.join("cycle2")).unwrap();
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("value.txt"), "text").unwrap();
        symlink(&shared, root.join("shared1")).unwrap();
        symlink(&shared, root.join("shared2")).unwrap();
        let snapshot = snapshot_from_vfs(&InstanceContext::new(), &vfs, &root)
            .unwrap()
            .unwrap();
        assert!(snapshot.metadata.context.active_directories.is_none());
        let reloaded = snapshot_from_vfs(&snapshot.metadata.context, &vfs, &root)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.children.len(), 8);
    }

    #[test]
    fn newly_created_initializer_is_not_hidden_by_old_directory_listing() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/root", VfsSnapshot::empty_dir())
            .unwrap();
        let vfs = Vfs::new(imfs);
        let original = snapshot_from_vfs(&InstanceContext::new(), &vfs, Path::new("/root"))
            .unwrap()
            .unwrap();
        // Absent init files aren't tracked. Change processing reaches the folder
        // through its own path, the nearest tracked ancestor of a new init file.
        assert_eq!(
            original.metadata.relevant_paths.first(),
            Some(&Path::new("/root").to_path_buf())
        );
        assert!(!original
            .metadata
            .relevant_paths
            .contains(&Path::new("/root/init.luau").to_path_buf()));
        vfs.write("/root/init.luau", "return 42").unwrap();
        let reloaded = snapshot_from_vfs(&original.metadata.context, &vfs, Path::new("/root"))
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.class_name, "ModuleScript");
        vfs.remove_file("/root/init.luau").unwrap();
        assert_eq!(
            snapshot_from_vfs(&reloaded.metadata.context, &vfs, Path::new("/root"))
                .unwrap()
                .unwrap()
                .class_name,
            "Folder"
        );
    }

    #[test]
    fn nested_initializers_use_their_own_entries() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([(
                "nested",
                VfsSnapshot::dir([
                    ("init.luau", VfsSnapshot::file("return {}")),
                    ("child.txt", VfsSnapshot::file("hello")),
                ]),
            )]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);
        let snapshot = snapshot_from_vfs(&InstanceContext::new(), &vfs, Path::new("/root"))
            .unwrap()
            .unwrap();

        assert_eq!(snapshot.metadata.middleware, Some(Middleware::Dir));
        assert_eq!(snapshot.children.len(), 1);
        let nested = &snapshot.children[0];
        assert_eq!(nested.name, "nested");
        assert_eq!(nested.class_name, "ModuleScript");
        assert_eq!(
            nested.metadata.middleware,
            Some(Middleware::ModuleScriptDir)
        );
        assert_eq!(nested.children.len(), 1);
        assert_eq!(nested.children[0].name, "child");
    }

    #[test]
    fn snapshot_wrapper_and_collected_entries_assign_middleware() {
        for (middleware, initializer, contents, class_name) in [
            (Middleware::Dir, None, "", "Folder"),
            (
                Middleware::ModuleScriptDir,
                Some("init.luau"),
                "return {}",
                "ModuleScript",
            ),
            (
                Middleware::CsvDir,
                Some("init.csv"),
                "Key,Source\nGreeting,Hello\n",
                "LocalizationTable",
            ),
        ] {
            let mut files = vec![("child.txt", VfsSnapshot::file("hello"))];
            if let Some(name) = initializer {
                files.push((name, VfsSnapshot::file(contents)));
            }
            let mut imfs = InMemoryFs::new();
            imfs.load_snapshot("/root", VfsSnapshot::dir(files))
                .unwrap();
            let vfs = Vfs::new(imfs);
            let context = InstanceContext::new();
            let path = Path::new("/root");
            let init_path = initializer.map_or_else(|| path.to_path_buf(), |name| path.join(name));
            let wrapped = middleware
                .snapshot(&context, &vfs, &init_path, "root")
                .unwrap()
                .unwrap();
            let collected = snapshot_from_vfs(&context, &vfs, path).unwrap().unwrap();

            for snapshot in [wrapped, collected] {
                assert_eq!(snapshot.metadata.middleware, Some(middleware));
                assert_eq!(snapshot.class_name, class_name);
                assert_eq!(snapshot.children.len(), 1);
                assert_eq!(snapshot.children[0].name, "child");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn initializer_lookup_follows_file_and_directory_symlinks() {
        use std::{fs, os::unix::fs::symlink};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let linked_root = temp.path().join("linked-root");
        let target = temp.path().join("module.luau");
        let directory_target = temp.path().join("directory");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&directory_target).unwrap();
        fs::write(&target, "return {}").unwrap();
        fs::write(root.join("child.txt"), "hello").unwrap();
        symlink(&target, root.join("init.luau")).unwrap();
        symlink(&directory_target, root.join("default.project.json")).unwrap();
        symlink(&root, &linked_root).unwrap();
        let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
        let entries = vfs
            .read_dir(&linked_root)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        let (middleware, _, init_path) =
            get_dir_middleware(&vfs, &linked_root, Some(&entries)).unwrap();

        assert_eq!(middleware, Middleware::ModuleScriptDir);
        assert_eq!(init_path, linked_root.join("init.luau"));
        let snapshot = snapshot_from_vfs(&InstanceContext::new(), &vfs, &linked_root)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.class_name, "ModuleScript");
        assert_eq!(
            snapshot.metadata.middleware,
            Some(Middleware::ModuleScriptDir),
        );
        assert!(snapshot.children.iter().any(|child| child.name == "child"));
    }

    #[test]
    fn legacy_client_init_selects_legacy_client_dir() {
        let middleware = middleware_for_dir(vec![(
            "init.local.luau",
            VfsSnapshot::file("print('client')"),
        )]);

        assert_eq!(middleware, Middleware::LegacyClientScriptDir);
    }
}
