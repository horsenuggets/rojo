use std::{path::Path, str};

use anyhow::Context as _;
use memofs::{DirEntry, Vfs};
use rbx_dom_weak::{
    types::{Enum, Variant},
    ustr, HashMapExt as _, UstrMap,
};

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

use super::{
    dir::{snapshot_dir_no_meta, syncback_dir_no_meta},
    luaurc::{self, ResolveInfo},
    meta_file::{AdjacentMetadata, DirectoryMetadata},
    PathExt as _,
};

#[derive(Debug)]
pub enum ScriptType {
    Server,
    Client,
    Module,
    Plugin,
    LegacyServer,
    LegacyClient,
    RunContextServer,
    RunContextClient,
}

/// The class and `RunContext` value that a script file's name declares by itself.
fn script_class(
    script_type: &ScriptType,
    emit_legacy_scripts: bool,
) -> (&'static str, Option<u32>) {
    let run_context = |name: &str| {
        rbx_reflection_database::get()
            .unwrap()
            .enums
            .get("RunContext")
            .expect("Unable to get RunContext enums!")
            .items
            .get(name)
            .copied()
    };
    match script_type {
        ScriptType::Server if emit_legacy_scripts => ("Script", run_context("Legacy")),
        ScriptType::Server | ScriptType::RunContextServer => ("Script", run_context("Server")),
        ScriptType::Client if emit_legacy_scripts => ("LocalScript", None),
        ScriptType::Client | ScriptType::RunContextClient => ("Script", run_context("Client")),
        ScriptType::Module => ("ModuleScript", None),
        ScriptType::Plugin => ("Script", run_context("Plugin")),
        ScriptType::LegacyServer => ("Script", run_context("Legacy")),
        ScriptType::LegacyClient => ("LocalScript", None),
    }
}

/// The `RunContext` a script middleware's file name already implies. A sidecar
/// repeating it would be redundant, such as `RunContext: Server` beside `.server.luau`.
fn implied_run_context(middleware: super::Middleware, emit_legacy_scripts: bool) -> Option<u32> {
    use super::Middleware;
    // Mirrors the script types each middleware snapshots with.
    let script_type = match middleware {
        Middleware::ServerScript => ScriptType::Server,
        Middleware::ClientScript => ScriptType::Client,
        Middleware::PluginScript | Middleware::PluginScriptDir => ScriptType::Plugin,
        Middleware::LegacyServerScript | Middleware::LegacyServerScriptDir => {
            ScriptType::LegacyServer
        }
        Middleware::RunContextServerScript | Middleware::ServerScriptDir => {
            ScriptType::RunContextServer
        }
        Middleware::RunContextClientScript | Middleware::ClientScriptDir => {
            ScriptType::RunContextClient
        }
        _ => return None,
    };
    script_class(&script_type, emit_legacy_scripts).1
}

/// Whether the destination file name already declares the instance's `RunContext`,
/// so its sidecar must not repeat it.
fn run_context_is_implied(snapshot: &SyncbackSnapshot) -> bool {
    // Suffix migrations leave the middleware unset and are dispatched by this choice.
    let middleware = snapshot
        .middleware
        .unwrap_or_else(|| crate::syncback::get_best_middleware(snapshot));
    let emit_legacy_scripts = snapshot
        .old_inst()
        .is_some_and(|old| old.metadata().context.emit_legacy_scripts);
    implied_run_context(middleware, emit_legacy_scripts).is_some_and(|value| {
        snapshot.new_inst().properties.get(&ustr("RunContext"))
            == Some(&Variant::Enum(Enum::from_u32(value)))
    })
}

/// Core routine for turning Lua files into snapshots.
pub fn snapshot_lua(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
    script_type: ScriptType,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let mut snapshot = snapshot_lua_source(context, vfs, path, name, script_type)?;
    AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;
    Ok(Some(snapshot))
}

/// Snapshots a Lua file without its adjacent metadata. Init scripts use this
/// because `<folder>/<folder>.meta.json` belongs to a same-named child script,
/// not to the folder, whose metadata lives in `init.meta.json`.
fn snapshot_lua_source(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
    script_type: ScriptType,
) -> anyhow::Result<InstanceSnapshot> {
    let (class_name, run_context) = script_class(&script_type, context.emit_legacy_scripts);

    let contents = vfs.read_to_string_lf_normalized(path)?;

    // Emit native anchored imports in DataModels and relative imports in models.
    // Keep the authored source separate so syncback can reverse this lowering.
    let mut import_inputs = None;
    let mut shared = context
        .import_input_scope
        .0
        .as_ref()
        .map(|cache| cache.lock().unwrap());
    let mut local = luaurc::ImportInputCache::new(context.captures_import_inputs());
    let inputs = shared.as_deref_mut().unwrap_or(&mut local);
    let source = if let Some(ref req_ctx) = context.require_context {
        let info = ResolveInfo {
            file_path: path,
            vfs,
            path_mappings: req_ctx.path_mappings(),
            project_root: &req_ctx.project_root,
        };

        let package_dependencies = context
            .package_mount
            .as_ref()
            .and_then(|(_, installed)| installed.parent());
        match luaurc::resolve_requires_with_inputs(
            &contents,
            &info,
            req_ctx.datamodel,
            package_dependencies,
            Some(&luaurc::PathMappingIndex::for_context(req_ctx)),
            inputs,
        ) {
            Ok((resolved, inputs)) => {
                import_inputs = inputs;
                resolved
            }
            Err(err) if err.is::<luaurc::UnmappedDataModelImport>() => {
                return Err(err)
                    .with_context(|| format!("Cannot resolve imports in {}", path.display()));
            }
            Err(err) => {
                // Syntax errors belong to the payload, not the import context. A
                // successful configuration read remains evidence even if lowering
                // failed. Read/configuration errors never populate this witness.
                import_inputs = inputs.witness_for(path);
                log::warn!("Failed to resolve requires in {}: {}", path.display(), err);
                contents.as_str().to_string()
            }
        }
    } else {
        if context.captures_import_inputs() {
            match inputs.capture_for(vfs, path) {
                Ok(witness) => import_inputs = witness,
                Err(error) => log::warn!(
                    "Cannot capture import inputs in {}. {error:#}",
                    path.display()
                ),
            }
        }
        contents.as_str().to_string()
    };
    drop(shared);

    let mut properties = UstrMap::with_capacity(2);
    properties.insert(ustr("Source"), source.as_str().into());

    if let Some(run_context) = run_context {
        properties.insert(
            ustr("RunContext"),
            Enum::from_u32(run_context.to_owned()).into(),
        );
    }

    let transform = (source != contents.as_str()).then(|| crate::snapshot::SourceTransform {
        original: contents.to_string(),
        emitted: source.clone(),
    });
    let mut snapshot = InstanceSnapshot::new()
        .name(name)
        .class_name(class_name)
        .properties(properties)
        .metadata(
            InstanceMetadata::new()
                .instigating_source(path)
                .relevant_paths(vec![vfs.canonicalize(path)?])
                .context(context),
        );
    snapshot.metadata.source_transform = transform;
    snapshot.metadata.import_inputs = import_inputs;
    Ok(snapshot)
}

/// Attempts to snapshot an 'init' Lua script contained inside of a folder with
/// the given name.
///
/// Scripts named `init.lua`, `init.server.lua`, or `init.client.lua` usurp
/// their parents, which acts similarly to `__init__.py` from the Python world.
pub fn snapshot_lua_init(
    context: &InstanceContext,
    vfs: &Vfs,
    init_path: &Path,
    name: &str,
    script_type: ScriptType,
    entries: Option<Vec<DirEntry>>,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let folder_path = init_path.parent().unwrap();
    let dir_snapshot = snapshot_dir_no_meta(context, vfs, folder_path, name, entries)?.unwrap();

    if dir_snapshot.class_name != "Folder" {
        anyhow::bail!(
            "init.lua, init.server.lua, init.client.lua, init.legacy.lua, \
             init.local.lua, and init.plugin.lua can \
             only be used if the instance produced by the containing \
             directory would be a Folder.\n\
             \n\
             The directory {} turned into an instance of class {}.",
            folder_path.display(),
            dir_snapshot.class_name
        );
    }

    let mut init_snapshot =
        snapshot_lua_source(context, vfs, init_path, &dir_snapshot.name, script_type)?;

    // Filter out the init file itself from the directory's children by its
    // source path. A different child may legitimately be named `init`.
    init_snapshot.children = dir_snapshot
        .children
        .into_iter()
        .filter(|child| {
            child
                .metadata
                .instigating_source
                .as_ref()
                .map(|source| source.path())
                != Some(init_path)
        })
        .collect();
    let source_transform = init_snapshot.metadata.source_transform.take();
    let import_inputs = init_snapshot.metadata.import_inputs.take();
    init_snapshot.metadata = dir_snapshot.metadata;
    init_snapshot.metadata.source_transform = source_transform;
    init_snapshot.metadata.import_inputs = import_inputs;
    track_init_file(&mut init_snapshot, init_path);

    DirectoryMetadata::read_and_apply_all(vfs, folder_path, &mut init_snapshot)?;

    Ok(Some(init_snapshot))
}

/// Records the init file that defines a directory instance, since the directory
/// middleware only tracks the directory itself.
pub(super) fn track_init_file(snapshot: &mut InstanceSnapshot, init_path: &Path) {
    let paths = &mut snapshot.metadata.relevant_paths;
    if let (Some(directory), Some(file_name)) = (paths.first(), init_path.file_name()) {
        let tracked = directory.join(file_name);
        paths.insert(1, tracked);
    }
}

pub fn syncback_lua<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    profiling::scope!("Rojo.SerializeScriptWrite");
    let new_inst = snapshot.new_inst();

    let contents = if let Some(Variant::String(source)) = new_inst.properties.get(&ustr("Source")) {
        match snapshot
            .old_inst()
            .and_then(|old| old.metadata().source_transform.as_ref())
        {
            Some(transform) => luaurc::restore_source(transform, source)
                .context(luaurc::SourceSerializationDeferred)?
                .into_bytes(),
            None => source.as_bytes().to_vec(),
        }
    } else {
        anyhow::bail!("Scripts must have a `Source` property that is a String")
    };
    snapshot.validate_script_destination(&snapshot.path)?;
    let mut fs_snapshot = FsSnapshot::new();
    fs_snapshot.add_file(&snapshot.path, contents);
    if let Some(old_source) = snapshot
        .old_inst()
        .as_ref()
        .and_then(crate::syncback::source_file)
    {
        if !snapshot.is_scoped()
            && old_source != snapshot.path
            && old_source.parent() == snapshot.path.parent()
            && old_source
                .extension()
                .is_some_and(|extension| extension == "lua" || extension == "luau")
        {
            snapshot.validate_script_migration(&old_source, &snapshot.path)?;
            fs_snapshot.remove_file(old_source);
        }
    }

    let meta = AdjacentMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    if let Some(mut meta) = meta {
        // Scripts have relatively few properties that we care about, so shifting
        // is fine.
        meta.properties.shift_remove(&ustr("Source"));
        if run_context_is_implied(snapshot) {
            meta.properties.shift_remove(&ustr("RunContext"));
        }

        let meta_path = snapshot
            .path
            .parent_err()?
            .join(format!("{}.meta.json", snapshot.file_stem()));
        if !meta.is_empty() || metadata_exists(snapshot.vfs(), &meta_path) {
            fs_snapshot.add_file(
                meta_path,
                crate::json::to_vec_pretty(&meta).context("cannot serialize metadata")?,
            );
        }
    }

    Ok(SyncbackReturn {
        fs_snapshot,
        // Scripts don't have a child!
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

pub fn syncback_lua_init<'sync>(
    script_type: ScriptType,
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();
    let mut path = snapshot.path.join(match script_type {
        ScriptType::Server => "init.server.luau",
        ScriptType::Client => "init.client.luau",
        ScriptType::Module => "init.luau",
        ScriptType::LegacyServer => "init.legacy.luau",
        ScriptType::LegacyClient => "init.local.luau",
        ScriptType::Plugin => "init.plugin.luau",
        _ => anyhow::bail!("syncback is not yet implemented for {script_type:?}"),
    });

    let contents = if let Some(Variant::String(source)) = new_inst.properties.get(&ustr("Source")) {
        match snapshot
            .old_inst()
            .and_then(|old| old.metadata().source_transform.as_ref())
        {
            Some(transform) => luaurc::restore_source(transform, source)
                .context(luaurc::SourceSerializationDeferred)?
                .into_bytes(),
            None => source.as_bytes().to_vec(),
        }
    } else {
        anyhow::bail!("Scripts must have a `Source` property that is a String")
    };

    if let Some(old_source) = snapshot
        .old_inst()
        .as_ref()
        .and_then(crate::syncback::source_file)
    {
        if old_source.parent() == Some(snapshot.path.as_path()) {
            if let Some(extension) = old_source
                .extension()
                .filter(|extension| *extension == "lua" || *extension == "luau")
            {
                path.set_extension(extension);
            }
        }
    }

    // Scoped writes are retargeted to their tracked initializer by the caller.
    if !snapshot.is_scoped() {
        snapshot.validate_script_destination(&path)?;
    }
    let mut dir_syncback = syncback_dir_no_meta(snapshot)?;
    dir_syncback.fs_snapshot.add_file(&path, contents);
    if let Some(old_source) = snapshot
        .old_inst()
        .as_ref()
        .and_then(crate::syncback::source_file)
    {
        if !snapshot.is_scoped() && old_source != path && old_source.parent() == path.parent() {
            snapshot.validate_script_migration(&old_source, &path)?;
            dir_syncback.fs_snapshot.remove_file(old_source);
        }
    }

    let meta = DirectoryMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    if let Some(mut meta) = meta {
        // Scripts have relatively few properties that we care about, so shifting
        // is fine.
        meta.properties.shift_remove(&ustr("Source"));
        if run_context_is_implied(snapshot) {
            meta.properties.shift_remove(&ustr("RunContext"));
        }

        let meta_path = snapshot.path.join("init.meta.json");
        if !meta.is_empty() || metadata_exists(snapshot.vfs(), &meta_path) {
            dir_syncback.fs_snapshot.add_file(
                meta_path,
                crate::json::to_vec_pretty(&meta)
                    .context("could not serialize new init.meta.json")?,
            );
        }
    }

    Ok(dir_syncback)
}

/// Returns whether metadata already exists at `path` or its `.jsonc` sibling.
/// Metadata with nothing left to store is normally skipped, but an existing
/// file must still be rewritten, or the values just cleared would stay on disk.
pub(super) fn metadata_exists(vfs: &Vfs, path: &Path) -> bool {
    [path.to_path_buf(), path.with_extension("jsonc")]
        .iter()
        .any(|candidate| {
            vfs.metadata(candidate)
                .is_ok_and(|metadata| metadata.is_file())
        })
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn class_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn plugin_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.plugin.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.plugin.lua"),
            "foo",
            ScriptType::Plugin,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_server_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_server_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_client_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.client.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.client.lua"),
            "foo",
            ScriptType::Client,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_client_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.client.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.client.lua"),
            "foo",
            ScriptType::Client,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([("init.lua", VfsSnapshot::file("Hello!"))]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.lua"),
            "root",
            ScriptType::Module,
            None,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_script_ignores_metadata_of_same_named_child() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([(
                "Saver",
                VfsSnapshot::dir([
                    ("init.server.luau", VfsSnapshot::file("-- parent")),
                    ("Saver.server.luau", VfsSnapshot::file("-- child")),
                    (
                        "Saver.meta.json",
                        VfsSnapshot::file(r#"{"properties": {"Disabled": true}}"#),
                    ),
                ]),
            )]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);

        let parent = crate::snapshot_middleware::snapshot_from_vfs(
            &InstanceContext::new(),
            &vfs,
            Path::new("/root/Saver"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(parent.class_name, "Script");
        assert_eq!(parent.properties.get(&ustr("Disabled")), None);
        assert_eq!(parent.children.len(), 1);
        assert_eq!(
            parent.children[0].properties.get(&ustr("Disabled")),
            Some(&Variant::Bool(true))
        );
    }

    #[test]
    fn init_module_from_vfs_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.lua", VfsSnapshot::file("Hello!")),
                (
                    "init.meta.json",
                    VfsSnapshot::file(r#"{"id": "manually specified"}"#),
                ),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.lua"),
            "root",
            ScriptType::Module,
            None,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_keeps_a_distinct_child_named_init() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.luau", VfsSnapshot::file("return {}")),
                ("init_.luau", VfsSnapshot::file("return {}")),
                ("init_.meta.json", VfsSnapshot::file(r#"{"name":"init"}"#)),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/root/init.luau"),
            "root",
            ScriptType::Module,
            None,
        )
        .unwrap()
        .unwrap();

        let child_names = instance_snapshot
            .children
            .iter()
            .map(|child| child.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(child_names, vec!["init"]);
    }

    #[test]
    fn init_plugin_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.plugin.luau", VfsSnapshot::file("print('hello')")),
                ("Helper.luau", VfsSnapshot::file("return {}")),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/root/init.plugin.luau"),
            "root",
            ScriptType::Plugin,
            None,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_legacy_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.legacy.luau", VfsSnapshot::file("print('hello')")),
                ("Helper.luau", VfsSnapshot::file("return {}")),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.legacy.luau"),
            "root",
            ScriptType::LegacyServer,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(instance_snapshot.name, "root");
        assert_eq!(instance_snapshot.class_name, "Script");

        let child_names = instance_snapshot
            .children
            .iter()
            .map(|child| child.name.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(child_names, vec!["Helper"]);
    }

    #[test]
    fn init_local_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.local.luau", VfsSnapshot::file("print('hello')")),
                ("Helper.luau", VfsSnapshot::file("return {}")),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.local.luau"),
            "root",
            ScriptType::LegacyClient,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(instance_snapshot.name, "root");
        assert_eq!(instance_snapshot.class_name, "LocalScript");

        let child_names = instance_snapshot
            .children
            .iter()
            .map(|child| child.name.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(child_names, vec!["Helper"]);
    }

    #[test]
    fn class_module_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_module_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_script_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_script_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_script_disabled() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/bar.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/bar.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "properties": {
                            "Disabled": true
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/bar.server.lua"),
            "bar",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_script_disabled() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/bar.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/bar.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "properties": {
                            "Disabled": true
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/bar.server.lua"),
            "bar",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }
}
