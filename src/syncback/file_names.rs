//! Contains logic for generating new file names for Instances based on their
//! middleware.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::Context;
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, WeakDom,
};

use crate::{snapshot::InstanceWithMeta, snapshot_middleware::Middleware};

use super::ModelTrees;

pub fn name_for_inst<'old>(
    middleware: Middleware,
    new_inst: &Instance,
    old_inst: Option<InstanceWithMeta<'old>>,
) -> anyhow::Result<Cow<'old, str>> {
    if let Some(old_inst) = old_inst {
        if let Some(source) = old_inst.metadata().relevant_paths.first() {
            let name = source
                .file_name()
                .and_then(|s| s.to_str())
                .context("sources on the file system should be valid unicode and not be stubs")?;
            Ok(canonical_script_source_name(
                middleware,
                source,
                &old_inst.metadata().context.sync_rules,
            )
            .unwrap_or(Cow::Borrowed(name)))
        } else {
            // This is technically not /always/ true, but we want to avoid
            // running syncback on anything that has no instigating source
            // anyway.
            anyhow::bail!(
                "members of 'old' trees should have an instigating source. Somehow, {} did not.",
                old_inst.name(),
            );
        }
    } else {
        Ok(match middleware {
            Middleware::Dir
            | Middleware::CsvDir
            | Middleware::ServerScriptDir
            | Middleware::ClientScriptDir
            | Middleware::LegacyServerScriptDir
            | Middleware::LegacyClientScriptDir
            | Middleware::PluginScriptDir
            | Middleware::ModuleScriptDir => Cow::Owned(new_inst.name.clone()),
            _ => {
                let extension = extension_for_middleware(middleware);
                let name = &new_inst.name;
                validate_file_name(name).with_context(|| {
                    format!("name '{name}' is not legal to write to the file system")
                })?;
                Cow::Owned(format!("{name}.{extension}"))
            }
        })
    }
}

/// Keeps a script's physical stem and source extension while replacing an
/// incompatible script-kind suffix. Directories retain their existing names.
pub(super) fn canonical_script_file_name(
    middleware: Middleware,
    name: &str,
) -> Option<Cow<'_, str>> {
    if middleware.is_dir() {
        return None;
    }
    let kind = script_kind_suffix(middleware)?;
    let (base, extension) = name.rsplit_once('.')?;
    if !matches!(extension, "lua" | "luau") {
        return None;
    }
    let stem = [".server", ".client", ".legacy", ".local", ".plugin"]
        .into_iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    let canonical = format!("{stem}{kind}.{extension}");
    Some(if canonical == name {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(canonical)
    })
}

/// Uses the forward rule's actual stem, so custom suffixes do not change names
/// or collapse distinct script sources onto the same destination.
pub(super) fn canonical_script_source_name<'a>(
    middleware: Middleware,
    source: &'a Path,
    rules: &[crate::snapshot::SyncRule],
) -> Option<Cow<'a, str>> {
    if middleware.is_dir() {
        return None;
    }
    let name = source.file_name()?.to_str()?;
    let kind = script_kind_suffix(middleware)?;
    let extension = source.extension()?.to_str()?;
    if !matches!(extension, "lua" | "luau") {
        return None;
    }
    let Some(rule) = rules
        .iter()
        .chain(crate::snapshot_middleware::default_sync_rules())
        .find(|rule| rule.matches(source))
    else {
        return canonical_script_file_name(middleware, name);
    };
    let stem = rule.file_name_for_path(source).ok()?;
    let canonical = format!("{stem}{kind}.{extension}");
    Some(if canonical == name {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(canonical)
    })
}

pub(super) fn script_kind_suffix(middleware: Middleware) -> Option<&'static str> {
    match middleware {
        Middleware::ServerScript
        | Middleware::RunContextServerScript
        | Middleware::ServerScriptDir => Some(".server"),
        Middleware::ClientScript
        | Middleware::RunContextClientScript
        | Middleware::ClientScriptDir => Some(".client"),
        Middleware::LegacyServerScript | Middleware::LegacyServerScriptDir => Some(".legacy"),
        Middleware::LegacyClientScript | Middleware::LegacyClientScriptDir => Some(".local"),
        Middleware::ModuleScript | Middleware::ModuleScriptDir => Some(""),
        Middleware::PluginScript | Middleware::PluginScriptDir => Some(".plugin"),
        _ => None,
    }
}

/// Returns the file name to use for a new Instance with the given middleware
/// and a stem from [`unique_child_stems`], which is already safe to write.
pub fn file_name_for_stem(middleware: Middleware, stem: &str) -> String {
    if middleware.is_dir() {
        stem.to_owned()
    } else {
        format!("{stem}.{}", extension_for_middleware(middleware))
    }
}

/// Returns the name Rojo gives an Instance when it reads `path` back with the
/// given middleware, before any `name` override from metadata is applied.
pub fn stem_for_path(middleware: Middleware, path: &Path) -> Option<&str> {
    let file_name = path.file_name()?.to_str()?;
    if middleware.is_dir() {
        Some(file_name)
    } else {
        file_name.strip_suffix(&format!(".{}", extension_for_middleware(middleware)))
    }
}

/// Returns the extension a provided piece of middleware is supposed to use.
pub fn extension_for_middleware(middleware: Middleware) -> &'static str {
    match middleware {
        Middleware::Csv => "csv",
        Middleware::JsonModel => "model.json",
        Middleware::Json => "json",
        Middleware::ServerScript | Middleware::RunContextServerScript => "server.luau",
        Middleware::ClientScript | Middleware::RunContextClientScript => "client.luau",
        Middleware::LegacyServerScript => "legacy.luau",
        Middleware::LegacyClientScript => "local.luau",
        Middleware::ModuleScript => "luau",
        Middleware::PluginScript => "plugin.luau",
        Middleware::Project => "project.json",
        Middleware::Rbxm => "rbxm",
        Middleware::Rbxmx => "rbxmx",
        Middleware::Toml => "toml",
        Middleware::Text => "txt",
        Middleware::Yaml => "yml",
        // These are manually specified and not `_` to guard against future
        // middleware additions missing this function.
        Middleware::Ignore => unimplemented!("syncback does not work on Ignore middleware"),
        Middleware::Dir
        | Middleware::CsvDir
        | Middleware::ServerScriptDir
        | Middleware::ClientScriptDir
        | Middleware::LegacyServerScriptDir
        | Middleware::LegacyClientScriptDir
        | Middleware::PluginScriptDir
        | Middleware::ModuleScriptDir => {
            unimplemented!("directory middleware requires special treatment")
        }
    }
}

/// A list of file names that are not valid on Windows.
const INVALID_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A list of all characters that are outright forbidden to be included
/// in a file's name.
const FORBIDDEN_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '|', '?', '*', '\\'];

/// Validates a provided file name to ensure it's allowed on the file system. An
/// error is returned if the name isn't allowed, indicating why.
/// This takes into account rules for Windows, MacOS, and Linux.
///
/// In practice however, these broadly overlap so the only unexpected behavior
/// is Windows, where there are 22 reserved names.
pub fn validate_file_name<S: AsRef<str>>(name: S) -> anyhow::Result<()> {
    let str = name.as_ref();

    if str.is_empty() {
        anyhow::bail!("file names cannot be empty")
    }
    if str.ends_with(' ') {
        anyhow::bail!("file names cannot end with a space")
    }
    if str.ends_with('.') {
        anyhow::bail!("file names cannot end with '.'")
    }

    for char in str.chars() {
        if FORBIDDEN_CHARS.contains(&char) {
            anyhow::bail!("file names cannot contain <, >, :, \", /, |, ?, *, or \\")
        } else if char.is_control() {
            anyhow::bail!("file names cannot contain control characters")
        }
    }

    let stem = str.split('.').next().unwrap();
    for forbidden in INVALID_WINDOWS_NAMES {
        if stem.eq_ignore_ascii_case(forbidden) {
            anyhow::bail!("files cannot be named {str}")
        }
    }

    Ok(())
}

/// Selects a script's representation from its class and execution context, not
/// from the historical meaning of `.server` and `.client` files.
pub fn script_middleware_for_instance(inst: &Instance) -> Option<Middleware> {
    match inst.class.as_str() {
        "LocalScript" => Some(Middleware::LegacyClientScript),
        "ModuleScript" => Some(Middleware::ModuleScript),
        "Script" => {
            let contexts = &rbx_reflection_database::get().unwrap().enums["RunContext"].items;
            let context = match inst.properties.get(&"RunContext".into()) {
                Some(Variant::Enum(context)) => context.to_u32(),
                _ => contexts["Legacy"],
            };
            Some(if context == contexts["Client"] {
                Middleware::RunContextClientScript
            } else if context == contexts["Server"] {
                Middleware::RunContextServerScript
            } else if context == contexts["Plugin"] {
                Middleware::PluginScript
            } else {
                Middleware::LegacyServerScript
            })
        }
        _ => None,
    }
}

/// Returns the middleware used for an Instance in `instancesAsDirectories`
/// mode, including the directory variant when it has children.
pub fn directory_middleware_for_instance(inst: &Instance) -> Middleware {
    let middleware =
        script_middleware_for_instance(inst).unwrap_or_else(|| match inst.class.as_str() {
            "StringValue" => Middleware::Text,
            "LocalizationTable" => Middleware::Csv,
            _ => Middleware::Dir,
        });

    if inst.children().is_empty() {
        middleware
    } else {
        directory_variant(middleware)
    }
}

pub(super) fn directory_variant(middleware: Middleware) -> Middleware {
    match middleware {
        Middleware::ServerScript | Middleware::RunContextServerScript => {
            Middleware::ServerScriptDir
        }
        Middleware::ClientScript | Middleware::RunContextClientScript => {
            Middleware::ClientScriptDir
        }
        Middleware::LegacyServerScript => Middleware::LegacyServerScriptDir,
        Middleware::LegacyClientScript => Middleware::LegacyClientScriptDir,
        Middleware::PluginScript => Middleware::PluginScriptDir,
        Middleware::ModuleScript => Middleware::ModuleScriptDir,
        Middleware::Csv => Middleware::CsvDir,
        Middleware::Text | Middleware::JsonModel => Middleware::Dir,
        other => other,
    }
}

/// Returns whether instances of this class are written as files rather than
/// directories when `instancesAsDirectories` is enabled.
fn is_file_class(class: &str) -> bool {
    matches!(class, "Script" | "LocalScript" | "ModuleScript")
}

/// Turns an Instance name into a file stem that every supported file system
/// accepts and that Rojo reads back as the same kind of Instance.
///
/// Forbidden and control characters become `_`, as do dots in the names of
/// script files (so `Foo.server` cannot turn into a server script). Names that
/// end in a space or dot, are empty, are reserved on Windows, or would collide
/// with an initializer path get adjusted.
fn sanitize_stem(name: &str, class: &str) -> String {
    let is_file = is_file_class(class);
    let mut stem: String = name
        .chars()
        .map(|char| {
            if FORBIDDEN_CHARS.contains(&char) || char.is_control() || (is_file && char == '.') {
                '_'
            } else {
                char
            }
        })
        .collect();

    let upper = stem.to_uppercase();
    let reserved = INVALID_WINDOWS_NAMES
        .iter()
        .any(|reserved| upper == *reserved || upper.starts_with(&format!("{reserved}.")));
    if reserved {
        // Windows reserves the component before the first dot, so appending
        // after an extension would not make names like `aux.txt` safe.
        stem.insert(0, '_');
    }

    let lower = stem.to_ascii_lowercase();
    let file_backed = matches!(
        class,
        "Script" | "LocalScript" | "ModuleScript" | "StringValue" | "LocalizationTable"
    );
    let initializer = (file_backed && lower == "init")
        || [
            "init.lua",
            "init.luau",
            "init.server.lua",
            "init.server.luau",
            "init.client.lua",
            "init.client.luau",
            "init.legacy.lua",
            "init.legacy.luau",
            "init.local.lua",
            "init.local.luau",
            "init.plugin.lua",
            "init.plugin.luau",
            "init.csv",
            "init.meta.json",
            "init.meta.jsonc",
            "default.project.json",
            "default.project.jsonc",
        ]
        .contains(&lower.as_str());

    if stem.is_empty() || stem.ends_with([' ', '.']) || initializer {
        stem.push('_');
    }

    stem
}

/// Returns the stem a new child of a directory is written with. None of the
/// entries it would occupy may already be in the directory, compared without
/// case, where `existing` holds the lowercased names of those entries. Taken
/// stems get `~2`, `~3`, and so on, like [`unique_child_stems`].
pub fn free_child_stem(
    child: &Instance,
    middleware: Middleware,
    existing: &HashSet<String>,
) -> String {
    let base = sanitize_stem(&child.name, &child.class);
    let mut counter = 1;
    loop {
        let stem = if counter == 1 {
            base.clone()
        } else {
            format!("{base}~{counter}")
        };
        let free = output_paths(middleware, &stem).iter().all(|path| {
            let entry = path.split('/').next().unwrap_or(path);
            !existing.contains(&entry.to_lowercase())
        });
        if free {
            return stem;
        }
        counter += 1;
    }
}

/// Returns every path a child can occupy, including its adjacent metadata.
fn output_paths(middleware: Middleware, stem: &str) -> Vec<String> {
    let mut paths = vec![file_name_for_stem(middleware, stem)];
    if middleware.is_dir() {
        paths.push(format!("{stem}/init.meta.json"));
    } else {
        // Both sidecars are applied when the file is read.
        paths.push(format!("{stem}.meta.json"));
        paths.push(format!("{stem}.meta.jsonc"));
    }
    paths
}

/// Orders sibling file names so the `~N` suffixes from [`unique_child_stems`]
/// compare numerically. Duplicates then come back in the order they had in the
/// place (`Model`, `Model~2`, ..., `Model~10`), which is what matching them
/// against Studio's children by order relies on. Other names sort as before.
pub fn sibling_order_key(file_name: &str) -> (Cow<'_, str>, u64) {
    if let Some(tilde) = file_name.rfind('~') {
        let after = &file_name[tilde + 1..];
        let digits_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let (digits, rest) = after.split_at(digits_end);
        // The suffix is followed by the extension, if any, never more of the name.
        if !digits.is_empty() && (rest.is_empty() || rest.starts_with('.')) {
            if let Ok(suffix) = digits.parse::<u64>() {
                let base = format!("{}{rest}", &file_name[..tilde]);
                return (Cow::Owned(base), suffix);
            }
        }
    }
    (Cow::Borrowed(file_name), 1)
}

/// Assigns each child of `parent` a file stem whose generated output paths are
/// safe and unique among its siblings, ignoring case. The first available stem
/// is kept, and later collisions get `~2`, `~3`, and so on.
///
/// Children that `model_trees` stores as model files reserve the paths of
/// those files, rather than the directories they would otherwise be.
pub fn unique_child_stems(
    dom: &WeakDom,
    parent: &Instance,
    model_trees: Option<&ModelTrees>,
) -> HashMap<Ref, String> {
    let mut taken = HashSet::with_capacity(parent.children().len() * 2);
    let mut next_suffix = HashMap::new();
    let mut stems = HashMap::with_capacity(parent.children().len());

    for child_ref in parent.children() {
        let child = dom
            .get_by_ref(*child_ref)
            .expect("children should be in the DOM");
        let base = sanitize_stem(&child.name, &child.class);
        let base_key = base.to_lowercase();
        let middleware = model_trees
            .and_then(|trees| trees.middleware(*child_ref))
            .unwrap_or_else(|| directory_middleware_for_instance(child));
        let mut counter = next_suffix.get(&base_key).copied().unwrap_or(1);

        loop {
            let stem = if counter == 1 {
                base.clone()
            } else {
                format!("{base}~{counter}")
            };
            let paths = output_paths(middleware, &stem);
            if paths
                .iter()
                .all(|path| !taken.contains(&path.to_lowercase()))
            {
                for path in paths {
                    taken.insert(path.to_lowercase());
                }
                next_suffix.insert(base_key, counter + 1);
                stems.insert(*child_ref, stem);
                break;
            }
            counter += 1;
        }
    }

    stems
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::InstanceBuilder;

    use crate::{project::ProjectNode, syncback::model_trees::CompiledModelTreeRules};

    fn free_stem(name: &str, class: &str, existing: &[&str]) -> String {
        let dom = WeakDom::new(InstanceBuilder::new(class).with_name(name));
        let existing = existing.iter().map(|name| name.to_lowercase()).collect();
        free_child_stem(
            dom.root(),
            directory_middleware_for_instance(dom.root()),
            &existing,
        )
    }

    #[test]
    fn free_child_stem_avoids_every_existing_entry_without_case() {
        assert_eq!(free_stem("Folder", "Folder", &[]), "Folder");
        assert_eq!(free_stem("Folder", "Folder", &["folder"]), "Folder~2");
        assert_eq!(
            free_stem("Folder", "Folder", &["FOLDER", "Folder~2"]),
            "Folder~3"
        );
        // A script's metadata sidecars count too, in either format.
        for sidecar in ["Mod.meta.json", "Mod.meta.jsonc"] {
            assert_eq!(free_stem("Mod", "ModuleScript", &[sidecar]), "Mod~2");
        }
        assert_eq!(free_stem("Mod", "ModuleScript", &["Mod"]), "Mod");
        // Names that aren't safe file names are sanitized first.
        assert_eq!(
            free_stem("a/b", "Folder", &[]),
            free_stem("a/b", "Folder", &[])
        );
        assert!(!free_stem("a/b", "Folder", &[]).contains('/'));
    }

    fn stems_for(children: &[(&str, &str)]) -> Vec<String> {
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let root = dom.root_ref();
        let refs: Vec<Ref> = children
            .iter()
            .map(|(name, class)| dom.insert(root, InstanceBuilder::new(*class).with_name(*name)))
            .collect();
        let stems = unique_child_stems(&dom, dom.root(), None);
        refs.iter()
            .map(|referent| stems[referent].clone())
            .collect()
    }

    #[test]
    fn model_trees_reserve_their_file_paths() {
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let root = dom.root_ref();
        let model = dom.insert(root, InstanceBuilder::new("Folder").with_name("FX"));
        let directory = dom.insert(root, InstanceBuilder::new("Folder").with_name("FX.rbxm"));
        let rules = CompiledModelTreeRules::new(
            &[serde_json::from_str(r#"{ "pattern": "FX" }"#).unwrap()],
            &ProjectNode::default(),
        )
        .unwrap();
        let trees = ModelTrees::find(&rules, &dom);

        // Without the rule both are directories, and their names don't clash.
        let plain = unique_child_stems(&dom, dom.root(), None);
        assert_eq!(plain[&model], "FX");
        assert_eq!(plain[&directory], "FX.rbxm");

        // With it, `FX` is written to `FX.rbxm`, which the directory must avoid.
        let stems = unique_child_stems(&dom, dom.root(), Some(&trees));
        assert_eq!(stems[&model], "FX");
        assert_eq!(stems[&directory], "FX.rbxm~2");
    }

    #[test]
    fn script_kinds_follow_class_and_run_context() {
        use rbx_dom_weak::types::Enum;

        let contexts = &rbx_reflection_database::get().unwrap().enums["RunContext"].items;
        for (class, context, extension, directory) in [
            ("ModuleScript", None, "luau", Middleware::ModuleScriptDir),
            (
                "LocalScript",
                None,
                "local.luau",
                Middleware::LegacyClientScriptDir,
            ),
            (
                "Script",
                None,
                "legacy.luau",
                Middleware::LegacyServerScriptDir,
            ),
            (
                "Script",
                Some("Legacy"),
                "legacy.luau",
                Middleware::LegacyServerScriptDir,
            ),
            (
                "Script",
                Some("Server"),
                "server.luau",
                Middleware::ServerScriptDir,
            ),
            (
                "Script",
                Some("Client"),
                "client.luau",
                Middleware::ClientScriptDir,
            ),
            (
                "Script",
                Some("Plugin"),
                "plugin.luau",
                Middleware::PluginScriptDir,
            ),
            (
                "LocalScript",
                Some("Client"),
                "local.luau",
                Middleware::LegacyClientScriptDir,
            ),
        ] {
            for has_children in [false, true] {
                let mut builder = InstanceBuilder::new(class);
                if let Some(context) = context {
                    builder =
                        builder.with_property("RunContext", Enum::from_u32(contexts[context]));
                }
                if has_children {
                    builder = builder.with_child(InstanceBuilder::new("Folder"));
                }
                let dom = WeakDom::new(builder);
                let actual = directory_middleware_for_instance(dom.root());
                if has_children {
                    assert_eq!(actual, directory, "{class} {context:?}");
                } else {
                    assert_eq!(
                        extension_for_middleware(actual),
                        extension,
                        "{class} {context:?}"
                    );
                    assert_eq!(
                        file_name_for_stem(actual, "Code"),
                        format!("Code.{extension}")
                    );
                }
            }
        }
    }

    #[test]
    fn existing_script_names_keep_stems_and_correct_the_kind() {
        for extension in ["lua", "luau"] {
            assert_eq!(
                canonical_script_file_name(
                    Middleware::LegacyClientScript,
                    &format!("Effect.client.{extension}")
                )
                .unwrap(),
                format!("Effect.local.{extension}")
            );
            assert_eq!(
                canonical_script_file_name(
                    Middleware::RunContextClientScript,
                    &format!("Effect.server.{extension}")
                )
                .unwrap(),
                format!("Effect.client.{extension}")
            );
            assert_eq!(
                canonical_script_file_name(
                    Middleware::LegacyServerScript,
                    &format!("Physical.extra.server.{extension}")
                )
                .unwrap(),
                format!("Physical.extra.legacy.{extension}")
            );
            let name = format!("Effect.local.{extension}");
            assert!(matches!(
                canonical_script_file_name(Middleware::LegacyClientScript, &name),
                Some(Cow::Borrowed(_))
            ));
        }
        assert_eq!(
            canonical_script_file_name(Middleware::LegacyClientScriptDir, "Effect.client.luau"),
            None
        );
        assert_eq!(
            canonical_script_file_name(Middleware::JsonModel, "Effect.model.json"),
            None
        );
    }

    fn sorted_by_sibling_order(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        names.sort_by_cached_key(|name| {
            let (base, suffix) = sibling_order_key(name);
            (base.into_owned(), suffix)
        });
        names
    }

    #[test]
    fn sibling_order_compares_duplicate_suffixes_numerically() {
        assert_eq!(
            sorted_by_sibling_order(&["Model~10", "Model~2", "Model", "Model~3"]),
            ["Model", "Model~2", "Model~3", "Model~10"]
        );
        assert_eq!(
            sorted_by_sibling_order(&[
                "Leaf~10.model.json",
                "Leaf.model.json",
                "Leaf~2.model.json"
            ]),
            ["Leaf.model.json", "Leaf~2.model.json", "Leaf~10.model.json"]
        );
        assert_eq!(
            sorted_by_sibling_order(&["Plane.003~10", "Plane.003", "Plane.003~2"]),
            ["Plane.003", "Plane.003~2", "Plane.003~10"]
        );
    }

    #[test]
    fn sibling_order_leaves_other_names_alone() {
        for name in ["a~b", "x~2y", "~", "Model~", "plain.txt", "init.meta.json"] {
            assert_eq!(sibling_order_key(name), (Cow::Borrowed(name), 1), "{name}");
        }
    }

    /// Stems from `unique_child_stems` must sort back into child order, since
    /// serve matches duplicate siblings against Studio by that order.
    #[test]
    fn duplicate_stems_sort_back_into_child_order() {
        let children: Vec<(&str, &str)> = (0..25).map(|_| ("Model", "Model")).collect();
        let stems = stems_for(&children);
        let mut shuffled = stems.clone();
        shuffled.sort();
        shuffled.reverse();
        let shuffled: Vec<&str> = shuffled.iter().map(String::as_str).collect();
        assert_eq!(sorted_by_sibling_order(&shuffled), stems);
    }

    #[test]
    fn unique_names_are_kept() {
        assert_eq!(
            stems_for(&[("Part", "Part"), ("Model", "Model"), ("Main", "Script")]),
            ["Part", "Model", "Main"]
        );
    }

    #[test]
    fn duplicates_get_numbered_in_order() {
        assert_eq!(
            stems_for(&[
                ("Part", "Part"),
                ("Part", "Part"),
                ("part", "Part"),
                ("Part", "Part")
            ]),
            ["Part", "Part~2", "part~3", "Part~4"]
        );
    }

    #[test]
    fn numbered_stems_skip_names_already_taken() {
        assert_eq!(
            stems_for(&[("A~2", "Part"), ("A", "Part"), ("A", "Part")]),
            ["A~2", "A", "A~3"]
        );
    }

    #[test]
    fn validation_rejects_empty_and_case_insensitive_reserved_device_names() {
        assert!(validate_file_name("").is_err());
        for reserved in INVALID_WINDOWS_NAMES {
            for name in [
                reserved.to_owned(),
                reserved.to_lowercase(),
                format!("{reserved}.txt"),
            ] {
                assert!(validate_file_name(&name).is_err(), "{name:?}");
            }
        }
        for name in ["Console", "auxiliary", "com10", "hello.world"] {
            assert!(validate_file_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn unsafe_names_are_sanitized() {
        assert_eq!(
            stems_for(&[
                ("a/b", "Part"),
                ("what?", "Part"),
                ("", "Part"),
                ("trailing ", "Part"),
                ("dots...", "Part"),
                ("CON", "Part"),
                ("aux.txt", "Part"),
                ("tab\there", "Part"),
            ]),
            [
                "a_b",
                "what_",
                "_",
                "trailing _",
                "dots..._",
                "_CON",
                "_aux.txt",
                "tab_here"
            ]
        );
    }

    #[test]
    fn script_names_cannot_change_their_middleware() {
        assert_eq!(
            stems_for(&[
                ("Foo.server", "ModuleScript"),
                ("init", "Script"),
                ("Init", "LocalScript"),
                ("init", "Folder"),
                ("Foo.server", "Folder"),
            ]),
            ["Foo_server", "init_", "Init_~2", "init", "Foo.server"]
        );
    }

    #[test]
    fn sanitized_names_still_dedupe() {
        assert_eq!(
            stems_for(&[("a?", "Part"), ("a_", "Part"), ("a*", "Part")]),
            ["a_", "a_~2", "a_~3"]
        );
    }

    #[test]
    fn output_paths_are_deduped_not_only_stems() {
        assert_eq!(
            stems_for(&[("Foo.luau", "Folder"), ("Foo", "ModuleScript")]),
            ["Foo.luau", "Foo~2"]
        );
        assert_eq!(
            stems_for(&[("Foo.meta.json", "Folder"), ("Foo", "ModuleScript")]),
            ["Foo.meta.json", "Foo~2"]
        );
    }

    #[test]
    fn initializer_names_are_reserved() {
        assert_eq!(
            stems_for(&[
                ("init", "LocalizationTable"),
                ("init.luau", "Folder"),
                ("default.project.json", "Folder"),
            ]),
            ["init_", "init.luau_", "default.project.json_"]
        );
    }

    #[test]
    fn many_duplicates_use_incrementing_suffixes() {
        let stems = stems_for(&vec![("Part", "Part"); 1000]);
        assert_eq!(stems[0], "Part");
        assert_eq!(stems[999], "Part~1000");
    }

    #[test]
    fn stems_are_deterministic() {
        let children = [("Part", "Part"), ("Part", "Part"), ("x/y", "Model")];
        assert_eq!(stems_for(&children), stems_for(&children));
    }

    #[test]
    fn stem_round_trips_through_file_name() {
        for middleware in [
            Middleware::Dir,
            Middleware::ModuleScript,
            Middleware::ServerScript,
        ] {
            let file_name = file_name_for_stem(middleware, "Part~2");
            assert_eq!(
                stem_for_path(middleware, Path::new(&file_name)),
                Some("Part~2")
            );
        }
    }
}
