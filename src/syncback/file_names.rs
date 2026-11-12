//! Contains logic for generating new file names for Instances based on their
//! middleware.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::Context;
use rbx_dom_weak::{types::Ref, Instance, WeakDom};

use crate::{snapshot::InstanceWithMeta, snapshot_middleware::Middleware};

pub fn name_for_inst<'old>(
    middleware: Middleware,
    new_inst: &Instance,
    old_inst: Option<InstanceWithMeta<'old>>,
) -> anyhow::Result<Cow<'old, str>> {
    if let Some(old_inst) = old_inst {
        if let Some(source) = old_inst.metadata().relevant_paths.first() {
            source
                .file_name()
                .and_then(|s| s.to_str())
                .map(Cow::Borrowed)
                .context("sources on the file system should be valid unicode and not be stubs")
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
        Middleware::ServerScript => "server.luau",
        Middleware::ClientScript => "client.luau",
        Middleware::ModuleScript => "luau",
        Middleware::PluginScript => "plugin.luau",
        Middleware::Project => "project.json",
        Middleware::Rbxm => "rbxm",
        Middleware::Rbxmx => "rbxmx",
        Middleware::Toml => "toml",
        Middleware::Text => "txt",
        Middleware::Yaml => "yml",

        Middleware::LegacyServerScript
        | Middleware::LegacyClientScript
        | Middleware::RunContextServerScript
        | Middleware::RunContextClientScript => {
            todo!("syncback does not work on the middleware {middleware:?} yet")
        }
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

    for forbidden in INVALID_WINDOWS_NAMES {
        if str == forbidden {
            anyhow::bail!("files cannot be named {str}")
        }
    }

    Ok(())
}

/// Returns the middleware used for an Instance in `instancesAsDirectories`
/// mode, including the directory variant when it has children.
pub fn directory_middleware_for_instance(inst: &Instance) -> Middleware {
    let middleware = match inst.class.as_str() {
        "Script" => Middleware::ServerScript,
        "LocalScript" => Middleware::ClientScript,
        "ModuleScript" => Middleware::ModuleScript,
        "StringValue" => Middleware::Text,
        "LocalizationTable" => Middleware::Csv,
        _ => Middleware::Dir,
    };

    if inst.children().is_empty() {
        middleware
    } else {
        match middleware {
            Middleware::ServerScript => Middleware::ServerScriptDir,
            Middleware::ClientScript => Middleware::ClientScriptDir,
            Middleware::ModuleScript => Middleware::ModuleScriptDir,
            Middleware::Csv => Middleware::CsvDir,
            Middleware::Text => Middleware::Dir,
            other => other,
        }
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

/// Returns every path a child can occupy, including its adjacent metadata.
fn output_paths(middleware: Middleware, stem: &str) -> Vec<String> {
    let mut paths = vec![file_name_for_stem(middleware, stem)];
    if middleware.is_dir() {
        paths.push(format!("{stem}/init.meta.json"));
    } else {
        paths.push(format!("{stem}.meta.json"));
    }
    paths
}

/// Assigns each child of `parent` a file stem whose generated output paths are
/// safe and unique among its siblings, ignoring case. The first available stem
/// is kept, and later collisions get `~2`, `~3`, and so on.
pub fn unique_child_stems(dom: &WeakDom, parent: &Instance) -> HashMap<Ref, String> {
    let mut taken = HashSet::with_capacity(parent.children().len() * 2);
    let mut next_suffix = HashMap::new();
    let mut stems = HashMap::with_capacity(parent.children().len());

    for child_ref in parent.children() {
        let child = dom
            .get_by_ref(*child_ref)
            .expect("children should be in the DOM");
        let base = sanitize_stem(&child.name, &child.class);
        let base_key = base.to_lowercase();
        let middleware = directory_middleware_for_instance(child);
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

    fn stems_for(children: &[(&str, &str)]) -> Vec<String> {
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let root = dom.root_ref();
        let refs: Vec<Ref> = children
            .iter()
            .map(|(name, class)| dom.insert(root, InstanceBuilder::new(*class).with_name(*name)))
            .collect();
        let stems = unique_child_stems(&dom, dom.root());
        refs.iter()
            .map(|referent| stems[referent].clone())
            .collect()
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
