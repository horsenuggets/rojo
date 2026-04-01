use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use full_moon::{
    ast::{self, Expression, Prefix, Suffix},
    tokenizer::{Token, TokenReference, TokenType},
};
use memofs::Vfs;
use serde::Deserialize;

/// Parsed `.luaurc` file content.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LuauRc {
    #[serde(default)]
    aliases: HashMap<String, String>,
}

/// Find the nearest `.luaurc` file by walking up from the given
/// directory. Returns the parsed aliases and the directory containing
/// the `.luaurc`.
fn find_luaurc(
    vfs: &Vfs,
    start_dir: &Path,
) -> anyhow::Result<Option<(HashMap<String, String>, PathBuf)>> {
    let mut current = start_dir.to_path_buf();

    loop {
        let luaurc_path = current.join(".luaurc");

        match vfs.read_to_string(&luaurc_path) {
            Ok(contents) => {
                let rc: LuauRc = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", luaurc_path.display()))?;
                return Ok(Some((rc.aliases, current)));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read {}", luaurc_path.display()));
            }
        }

        if !current.pop() {
            break;
        }
    }

    Ok(None)
}

/// Information needed to resolve requires for a single script file.
pub struct ResolveInfo<'a> {
    /// The filesystem path of the script file.
    pub file_path: &'a Path,
    /// The VFS for reading .luaurc files.
    pub vfs: &'a Vfs,
    /// Project tree `$path` mappings for DataModel-aware path
    /// computation. Maps filesystem paths (relative to project root)
    /// to DataModel path segments.
    pub path_mappings: &'a [(PathBuf, Vec<String>)],
    /// The project root directory.
    pub project_root: &'a Path,
}

/// Resolve `.luaurc` alias-based requires in the given Luau source
/// code. Transforms `require("@alias/path")` into relative
/// require-by-string paths like `require("../../Packages/path")`.
///
/// Returns the transformed source, or the original if no changes were
/// needed.
pub fn resolve_requires(source: &str, info: &ResolveInfo) -> anyhow::Result<String> {
    let file_dir = info
        .file_path
        .parent()
        .context("script file has no parent directory")?;

    let luaurc = find_luaurc(info.vfs, file_dir)?;

    let aliases = match &luaurc {
        Some((aliases, _)) => aliases,
        None => return Ok(source.to_string()),
    };

    if aliases.is_empty() {
        return Ok(source.to_string());
    }

    let luaurc_dir = luaurc.as_ref().unwrap().1.as_path();

    // Quick check: skip parsing if no custom alias strings present
    if !aliases.keys().any(|k| source.contains(&format!("@{k}"))) {
        return Ok(source.to_string());
    }

    // Init scripts usurp their parent folder. The script becomes
    // the folder, so its "current directory" for require-by-string
    // is the folder's parent, not the folder itself.
    let is_init = info
        .file_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.starts_with("init."))
        .unwrap_or(false);

    let replacements = find_require_replacements(
        source,
        file_dir,
        aliases,
        luaurc_dir,
        info.path_mappings,
        info.project_root,
        is_init,
    )?;

    if replacements.is_empty() {
        return Ok(source.to_string());
    }

    // Apply replacements in reverse order to preserve byte offsets
    let mut result = source.to_string();
    let mut sorted = replacements;
    sorted.sort_by(|a, b| b.start.cmp(&a.start));

    for replacement in sorted {
        result.replace_range(replacement.start..replacement.end, &replacement.new_text);
    }

    Ok(result)
}

/// A text replacement to apply to the source.
struct Replacement {
    start: usize,
    end: usize,
    new_text: String,
}

/// Compute the relative require-by-string path from a script's
/// directory to an alias target path, with an optional sub-path.
///
/// When `path_mappings` are available (from the project tree), paths
/// are projected into the DataModel tree before computing the
/// relative path. This accounts for project tree remappings like
/// `"Plugin": { "$path": "Source/Plugin" }` where the filesystem
/// has an extra directory level that doesn't exist in the DataModel.
fn compute_relative_require(
    script_dir: &Path,
    alias_target: &Path,
    sub_path: Option<&str>,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> String {
    let require_path = if !path_mappings.is_empty() {
        compute_datamodel_relative(
            script_dir,
            alias_target,
            path_mappings,
            project_root,
            is_init,
        )
    } else {
        compute_filesystem_relative(script_dir, alias_target)
    };

    match sub_path {
        Some(sub) if require_path.ends_with('/') => format!("{require_path}{sub}"),
        Some(sub) => format!("{require_path}/{sub}"),
        None => require_path,
    }
}

/// Compute the relative path using DataModel tree positions derived
/// from the project tree `$path` mappings.
fn compute_datamodel_relative(
    script_dir: &Path,
    alias_target: &Path,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> String {
    let mut script_dm = to_datamodel_path(script_dir, path_mappings, project_root);
    let target_dm = to_datamodel_path(alias_target, path_mappings, project_root);

    // Init scripts usurp their parent folder. The script IS the
    // folder, so its "current directory" for require-by-string is
    // the folder's parent (one level up in the DataModel).
    if is_init {
        script_dm.pop();
    }

    // Find common prefix length
    let common_len = script_dm
        .iter()
        .zip(target_dm.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Number of ../ needed
    let up_count = script_dm.len() - common_len;

    let mut parts = Vec::new();
    for _ in 0..up_count {
        parts.push("..".to_string());
    }
    for segment in &target_dm[common_len..] {
        parts.push(segment.clone());
    }

    let result = parts.join("/");
    if result.starts_with("..") {
        result
    } else {
        format!("./{result}")
    }
}

/// Convert a filesystem path to DataModel path segments using the
/// project tree mappings. Falls back to the project-root-relative
/// filesystem path if no mapping matches.
fn to_datamodel_path(
    fs_path: &Path,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
) -> Vec<String> {
    // Make the path relative to the project root
    let relative = fs_path.strip_prefix(project_root).unwrap_or(fs_path);

    // Find the best matching $path mapping (longest match first,
    // since mappings are sorted by length descending)
    for (mount_fs, mount_dm) in path_mappings {
        if let Ok(remainder) = relative.strip_prefix(mount_fs) {
            let mut dm_path = mount_dm.clone();
            for component in remainder.components() {
                dm_path.push(component.as_os_str().to_string_lossy().into_owned());
            }
            return dm_path;
        }
    }

    // Fallback: use the filesystem path as-is
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// Compute the relative path using pure filesystem distances.
/// Used as fallback when no project tree mappings are available.
fn compute_filesystem_relative(script_dir: &Path, alias_target: &Path) -> String {
    match diff_paths(alias_target, script_dir) {
        Some(rel) => {
            let rel_str = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");

            if rel_str.starts_with("..") {
                rel_str
            } else if rel_str.is_empty() {
                ".".to_string()
            } else {
                format!("./{rel_str}")
            }
        }
        None => alias_target.to_string_lossy().into_owned(),
    }
}

/// Compute a relative path from `base` to `target`.
///
/// This is equivalent to `pathdiff::diff_paths` but works with
/// logical paths (no filesystem access needed), handling the
/// InMemoryFs paths used in tests.
fn diff_paths(target: &Path, base: &Path) -> Option<PathBuf> {
    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();

    // Find common prefix length
    let common_len = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Number of ../ needed = remaining base components
    let up_count = base_components.len() - common_len;

    let mut result = PathBuf::new();
    for _ in 0..up_count {
        result.push("..");
    }

    // Append remaining target components
    for component in &target_components[common_len..] {
        result.push(component.as_os_str());
    }

    Some(result)
}

/// Scan the source for `require("@...")` calls and compute
/// replacements using the full_moon AST.
fn find_require_replacements(
    source: &str,
    script_dir: &Path,
    aliases: &HashMap<String, String>,
    luaurc_dir: &Path,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> anyhow::Result<Vec<Replacement>> {
    let ast = full_moon::parse(source)
        .map_err(|e| anyhow::anyhow!("failed to parse Luau source: {e:?}"))?;

    let mut replacements = Vec::new();

    visit_block(ast.nodes(), &mut |call, content| {
        if !content.starts_with('@') {
            return;
        }

        let call_start = call_byte_start(call);
        let call_end = call_byte_end(call);

        if let Some(replacement) = resolve_alias_require(
            &content,
            script_dir,
            aliases,
            luaurc_dir,
            call_start,
            call_end,
            path_mappings,
            project_root,
            is_init,
        ) {
            replacements.push(replacement);
        }
    });

    Ok(replacements)
}

/// Visit a block, looking for require() calls with string arguments.
fn visit_block(block: &ast::Block, callback: &mut dyn FnMut(&ast::FunctionCall, String)) {
    for stmt in block.stmts() {
        visit_stmt(stmt, callback);
    }
    if let Some(last) = block.last_stmt() {
        if let ast::LastStmt::Return(ret) = last {
            for expr in ret.returns() {
                visit_expr(expr, callback);
            }
        }
    }
}

fn visit_stmt(stmt: &ast::Stmt, callback: &mut dyn FnMut(&ast::FunctionCall, String)) {
    match stmt {
        ast::Stmt::FunctionCall(call) => {
            check_function_call(call, callback);
        }
        ast::Stmt::LocalAssignment(local) => {
            for expr in local.expressions() {
                visit_expr(expr, callback);
            }
        }
        ast::Stmt::Assignment(assign) => {
            for expr in assign.expressions() {
                visit_expr(expr, callback);
            }
        }
        ast::Stmt::Do(do_stmt) => {
            visit_block(do_stmt.block(), callback);
        }
        ast::Stmt::If(if_stmt) => {
            visit_block(if_stmt.block(), callback);
            if let Some(else_ifs) = if_stmt.else_if() {
                for else_if in else_ifs {
                    visit_block(else_if.block(), callback);
                }
            }
            if let Some(else_block) = if_stmt.else_block() {
                visit_block(else_block, callback);
            }
        }
        ast::Stmt::While(while_stmt) => {
            visit_block(while_stmt.block(), callback);
        }
        ast::Stmt::Repeat(repeat_stmt) => {
            visit_block(repeat_stmt.block(), callback);
        }
        ast::Stmt::NumericFor(for_stmt) => {
            visit_block(for_stmt.block(), callback);
        }
        ast::Stmt::GenericFor(for_stmt) => {
            visit_block(for_stmt.block(), callback);
        }
        _ => {}
    }
}

fn visit_expr(expr: &Expression, callback: &mut dyn FnMut(&ast::FunctionCall, String)) {
    match expr {
        Expression::FunctionCall(call) => {
            check_function_call(call, callback);
        }
        Expression::Parentheses { expression, .. } => {
            visit_expr(expression, callback);
        }
        _ => {}
    }
}

/// Check if a function call is `require("@...")` and if so, invoke
/// the callback with the call and the string content.
fn check_function_call(
    call: &ast::FunctionCall,
    callback: &mut dyn FnMut(&ast::FunctionCall, String),
) {
    let is_require = match call.prefix() {
        Prefix::Name(token) => token.token().to_string() == "require",
        _ => false,
    };

    if !is_require {
        return;
    }

    let suffixes: Vec<_> = call.suffixes().collect();
    if suffixes.len() != 1 {
        return;
    }

    let content = match &suffixes[0] {
        Suffix::Call(ast::Call::AnonymousCall(ast::FunctionArgs::Parentheses {
            arguments,
            parentheses: _,
        })) => {
            let args: Vec<_> = arguments.iter().collect();
            if args.len() != 1 {
                return;
            }
            extract_string_from_expr(&args[0])
        }
        Suffix::Call(ast::Call::AnonymousCall(ast::FunctionArgs::String(string_token))) => {
            extract_string_from_token(string_token)
        }
        _ => return,
    };

    if let Some(content) = content {
        if content.starts_with('@') {
            callback(call, content);
        }
    }
}

/// Extract string content from an Expression that is a string
/// literal.
fn extract_string_from_expr(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(token_ref) => extract_string_from_token(token_ref),
        _ => None,
    }
}

/// Extract the content of a string literal from a TokenReference.
fn extract_string_from_token(token_ref: &TokenReference) -> Option<String> {
    match token_ref.token().token_type() {
        TokenType::StringLiteral { literal, .. } => Some(literal.to_string()),
        _ => None,
    }
}

/// Get the byte offset of the start of a function call.
fn call_byte_start(call: &ast::FunctionCall) -> usize {
    match call.prefix() {
        Prefix::Name(token) => token.token().start_position().bytes(),
        _ => 0,
    }
}

/// Get the byte offset of the end of a function call.
fn call_byte_end(call: &ast::FunctionCall) -> usize {
    if let Some(last_suffix) = call.suffixes().last() {
        suffix_byte_end(last_suffix)
    } else {
        0
    }
}

/// Get the byte end of a suffix.
fn suffix_byte_end(suffix: &Suffix) -> usize {
    match suffix {
        Suffix::Call(ast::Call::AnonymousCall(func_args)) => func_args_byte_end(func_args),
        Suffix::Call(ast::Call::MethodCall(method)) => func_args_byte_end(method.args()),
        Suffix::Index(ast::Index::Brackets { brackets, .. }) => token_end(brackets.tokens().1),
        Suffix::Index(ast::Index::Dot { name, .. }) => token_end(name.token()),
        _ => 0,
    }
}

/// Get the byte end of function arguments.
fn func_args_byte_end(args: &ast::FunctionArgs) -> usize {
    match args {
        ast::FunctionArgs::Parentheses { parentheses, .. } => token_end(parentheses.tokens().1),
        ast::FunctionArgs::String(token) => token_end(token.token()),
        ast::FunctionArgs::TableConstructor(table) => token_end(table.braces().tokens().1),
        _ => 0,
    }
}

/// Get the byte position at the end of a token.
fn token_end(token: &Token) -> usize {
    token.end_position().bytes()
}

/// Resolve a single alias require and produce a Replacement.
fn resolve_alias_require(
    alias_path: &str,
    script_dir: &Path,
    aliases: &HashMap<String, String>,
    luaurc_dir: &Path,
    call_start: usize,
    call_end: usize,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> Option<Replacement> {
    let without_at = &alias_path[1..];

    let (alias_name, sub_path) = match without_at.find('/') {
        Some(idx) => (&without_at[..idx], Some(&without_at[idx + 1..])),
        None => (without_at, None),
    };

    // @self is natively supported by Roblox's require-by-string
    if alias_name == "self" {
        return None;
    }

    if let Some(alias_target) = aliases.get(alias_name) {
        let alias_fs_path = luaurc_dir.join(alias_target);

        let require_path = compute_relative_require(
            script_dir,
            &alias_fs_path,
            sub_path,
            path_mappings,
            project_root,
            is_init,
        );

        Some(Replacement {
            start: call_start,
            end: call_end,
            new_text: format!(r#"require("{require_path}")"#),
        })
    } else {
        log::warn!("Unknown alias @{} in {}", alias_name, script_dir.display());
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn relative_path_sibling() {
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin"),
            Path::new("/project/Packages"),
            Some("Fusion"),
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../../Packages/Fusion");
    }

    #[test]
    fn relative_path_same_level() {
        let result = compute_relative_require(
            Path::new("/project/Source"),
            Path::new("/project/Packages"),
            Some("Fusion"),
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../Packages/Fusion");
    }

    #[test]
    fn relative_path_bare_alias() {
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin"),
            Path::new("/project/Source/RbxPackageLoader"),
            None,
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../RbxPackageLoader");
    }

    #[test]
    fn relative_path_child() {
        let result = compute_relative_require(
            Path::new("/project/Source"),
            Path::new("/project/Source/Lib"),
            None,
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "./Lib");
    }

    #[test]
    fn relative_path_with_project_mappings() {
        // Project maps Source/Plugin -> Plugin and Packages -> Packages
        let mappings = vec![
            (PathBuf::from("Source/Plugin"), vec!["Plugin".to_string()]),
            (
                PathBuf::from("Source/RbxPackageLoader"),
                vec!["RbxPackageLoader".to_string()],
            ),
            (PathBuf::from("Packages"), vec!["Packages".to_string()]),
        ];

        // From Source/Plugin/ to Packages/ should be ../Packages
        // (not ../../Packages as filesystem would compute)
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin"),
            Path::new("/project/Packages"),
            Some("Fusion"),
            &mappings,
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../Packages/Fusion");
    }

    #[test]
    fn relative_path_with_project_mappings_sibling() {
        let mappings = vec![
            (PathBuf::from("Source/Plugin"), vec!["Plugin".to_string()]),
            (
                PathBuf::from("Source/RbxPackageLoader"),
                vec!["RbxPackageLoader".to_string()],
            ),
        ];

        // From Source/Plugin/ to Source/RbxPackageLoader/ should be
        // ../RbxPackageLoader (same as without mappings since both
        // are at the same DataModel depth)
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin"),
            Path::new("/project/Source/RbxPackageLoader"),
            None,
            &mappings,
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../RbxPackageLoader");
    }

    #[test]
    fn relative_path_nested_in_mapped_dir() {
        let mappings = vec![
            (PathBuf::from("Source/Plugin"), vec!["Plugin".to_string()]),
            (PathBuf::from("Packages"), vec!["Packages".to_string()]),
        ];

        // From Source/Plugin/Components/ (deeper inside mapped dir)
        // to Packages/Fusion
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin/Components"),
            Path::new("/project/Packages"),
            Some("Fusion"),
            &mappings,
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../../Packages/Fusion");
    }

    #[test]
    fn relative_path_init_with_project_mappings() {
        let mappings = vec![
            (PathBuf::from("Source/Plugin"), vec!["Plugin".to_string()]),
            (PathBuf::from("Packages"), vec!["Packages".to_string()]),
        ];

        // Init file at Source/Plugin/init.plugin.luau usurps Plugin.
        // Its "current directory" is Plugin's parent (root), so
        // Packages is a sibling: ./Packages/Fusion
        let result = compute_relative_require(
            Path::new("/project/Source/Plugin"),
            Path::new("/project/Packages"),
            Some("Fusion"),
            &mappings,
            Path::new("/project"),
            true,
        );
        assert_eq!(result, "./Packages/Fusion");
    }

    #[test]
    fn self_alias_left_untouched() {
        let source = r#"local Assets = require("@self/Assets")"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Plugin/init.plugin.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(result, source);
    }

    #[test]
    fn resolve_custom_alias() {
        let source = r#"local Fusion = require("@packages/Fusion")"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Plugin/init.plugin.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[(
                "/project/.luaurc",
                r#"{"aliases": {"packages": "Packages"}}"#,
            )]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(result, r#"local Fusion = require("../../Packages/Fusion")"#);
    }

    #[test]
    fn resolve_bare_alias() {
        let source = r#"local Loader = require("@rbxpackageloader")"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Plugin/init.plugin.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[(
                "/project/.luaurc",
                r#"{"aliases": {"rbxpackageloader": "Source/RbxPackageLoader"}}"#,
            )]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(result, r#"local Loader = require("../RbxPackageLoader")"#);
    }

    #[test]
    fn leaves_relative_requires_untouched() {
        let source = r#"local Foo = require("./Foo")"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Bar.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(result, source);
    }

    #[test]
    fn leaves_wally_requires_untouched() {
        let source = r#"local Promise = require(script.Parent._Index["promise"])"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/init.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(result, source);
    }

    fn make_test_vfs(files: &[(&str, &str)]) -> Vfs {
        use memofs::{InMemoryFs, VfsSnapshot};

        let mut imfs = InMemoryFs::new();
        for (path, content) in files {
            imfs.load_snapshot(path, VfsSnapshot::file(*content))
                .unwrap();
        }
        Vfs::new(imfs)
    }

    // ---- Integration tests using ServeSession ----

    use memofs::{InMemoryFs, VfsSnapshot};
    use rbx_dom_weak::{types::Variant, ustr};

    use crate::serve_session::ServeSession;

    /// Helper to get the Source property of an instance by name,
    /// searching all descendants of the root.
    fn find_source_by_name(session: &ServeSession, name: &str) -> Option<String> {
        let tree = session.tree();
        let root_id = tree.get_root_id();
        find_source_recursive(&tree, root_id, name)
    }

    fn find_source_recursive(
        tree: &crate::snapshot::RojoTree,
        id: rbx_dom_weak::types::Ref,
        name: &str,
    ) -> Option<String> {
        let inst = tree.get_instance(id)?;

        if inst.name() == name {
            if let Some(Variant::String(source)) = inst.properties().get(&ustr("Source")) {
                return Some(source.clone());
            }
        }

        for &child_id in inst.children() {
            if let Some(result) = find_source_recursive(tree, child_id, name) {
                return Some(result);
            }
        }

        None
    }

    /// Build a VFS with a complete project structure and create a
    /// ServeSession from it.
    fn create_test_session(files: Vec<(&str, VfsSnapshot)>) -> ServeSession {
        let mut imfs = InMemoryFs::new();
        for (path, snapshot) in files {
            imfs.load_snapshot(path, snapshot).unwrap();
        }
        let vfs = Vfs::new(imfs);
        ServeSession::new(vfs, Path::new("/project")).unwrap()
    }

    #[test]
    fn integration_self_alias_left_untouched() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Source": {
                                    "$path": "src"
                                }
                            }
                        }"#,
                    ),
                ),
                (".luaurc", VfsSnapshot::file(r#"{"aliases": {}}"#)),
                (
                    "src",
                    VfsSnapshot::dir([
                        (
                            "init.luau",
                            VfsSnapshot::file(
                                r#"local Child = require("@self/Child")
return {}"#,
                            ),
                        ),
                        ("Child.luau", VfsSnapshot::file("return {}")),
                    ]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Source").expect("should find Source script");
        assert!(
            source.contains(r#"require("@self/Child")"#),
            "@self should be left untouched, got: {source}"
        );
    }

    #[test]
    fn integration_custom_alias() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Packages": {
                                    "$path": "Packages"
                                },
                                "Plugin": {
                                    "$path": "Source/Plugin"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases": {"packages": "Packages"}}"#),
                ),
                (
                    "Packages",
                    VfsSnapshot::dir([("Fusion.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([(
                        "Plugin",
                        VfsSnapshot::dir([(
                            "init.luau",
                            VfsSnapshot::file(
                                r#"local Fusion = require("@packages/Fusion")
return {}"#,
                            ),
                        )]),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Plugin").expect("should find Plugin script");
        // Plugin maps to DataModel root child, Packages likewise.
        // DataModel relative: ../Packages/Fusion
        assert!(
            source.contains(r#"require("./Packages/Fusion")"#),
            "alias should resolve to relative path, got: {source}"
        );
    }

    #[test]
    fn integration_init_plugin_alias() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Packages": {
                                    "$path": "Packages"
                                },
                                "Plugin": {
                                    "$path": "Source/Plugin"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases": {"packages": "Packages"}}"#),
                ),
                (
                    "Packages",
                    VfsSnapshot::dir([("Fusion.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([(
                        "Plugin",
                        VfsSnapshot::dir([(
                            "init.plugin.luau",
                            VfsSnapshot::file(
                                r#"local Fusion = require("@packages/Fusion")
return {}"#,
                            ),
                        )]),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Plugin").expect("should find Plugin script");
        // init.plugin.luau usurps the folder. With project tree
        // mappings, the path is DataModel-relative: ../Packages/Fusion
        assert!(
            source.contains(r#"require("./Packages/Fusion")"#),
            "init.plugin.luau alias should resolve to relative path, got: {source}"
        );
    }

    #[test]
    fn integration_remapped_alias() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Plugin": {
                                    "$path": "Source/Plugin"
                                },
                                "RbxPackageLoader": {
                                    "$path": "Source/RbxPackageLoader"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(
                        r#"{"aliases": {
                            "rbxpackageloader": "Source/RbxPackageLoader"
                        }}"#,
                    ),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([
                        (
                            "Plugin",
                            VfsSnapshot::dir([(
                                "init.luau",
                                VfsSnapshot::file(
                                    r#"local Loader = require("@rbxpackageloader")
return {}"#,
                                ),
                            )]),
                        ),
                        (
                            "RbxPackageLoader",
                            VfsSnapshot::dir([("init.luau", VfsSnapshot::file("return {}"))]),
                        ),
                    ]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Plugin").expect("should find Plugin script");
        // Plugin at Source/Plugin/, target at
        // Source/RbxPackageLoader/
        // Relative: ../RbxPackageLoader
        assert!(
            source.contains(r#"require("./RbxPackageLoader")"#),
            "remapped alias should resolve to relative path, got: {source}"
        );
    }

    #[test]
    fn integration_relative_and_wally_untouched() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Source": {
                                    "$path": "src"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "Script.luau",
                        VfsSnapshot::file(
                            r#"local A = require("./Sibling")
local B = require(script.Parent._Index["promise"])
return {}"#,
                        ),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Script").expect("should find Script");
        assert!(
            source.contains(r#"require("./Sibling")"#),
            "relative require should be untouched, got: {source}"
        );
        assert!(
            source.contains(r#"require(script.Parent._Index["promise"])"#),
            "Wally require should be untouched, got: {source}"
        );
    }

    #[test]
    fn integration_cross_service_alias() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "DataModel",
                                "ReplicatedStorage": {
                                    "$className": "ReplicatedStorage",
                                    "Packages": {
                                        "$path": "Packages"
                                    }
                                },
                                "ServerScriptService": {
                                    "$className": "ServerScriptService",
                                    "Server": {
                                        "$path": "src/server"
                                    }
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases": {"packages": "Packages"}}"#),
                ),
                (
                    "Packages",
                    VfsSnapshot::dir([("SharedLib.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "server",
                        VfsSnapshot::dir([(
                            "Main.server.luau",
                            VfsSnapshot::file(r#"local Lib = require("@packages/SharedLib")"#),
                        )]),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Main").expect("should find Main script");
        // DataModel: ServerScriptService.Server.Main -> ReplicatedStorage.Packages.SharedLib
        // Relative: ../../ReplicatedStorage/Packages/SharedLib
        assert!(
            source.contains(r#"require("../../ReplicatedStorage/Packages/SharedLib")"#),
            "cross-service alias should resolve to relative path, got: {source}"
        );
    }

    #[test]
    fn integration_multiple_requires() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Packages": {
                                    "$path": "Packages"
                                },
                                "Source": {
                                    "$path": "Source"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases": {"packages": "Packages"}}"#),
                ),
                (
                    "Packages",
                    VfsSnapshot::dir([
                        ("Fusion.luau", VfsSnapshot::file("return {}")),
                        ("Roact.luau", VfsSnapshot::file("return {}")),
                    ]),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([(
                        "init.luau",
                        VfsSnapshot::file(
                            r#"local Fusion = require("@packages/Fusion")
local Roact = require("@packages/Roact")
local Child = require("@self/SubModule")
return {}"#,
                        ),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Source").expect("should find Source script");

        assert!(
            source.contains(r#"require("./Packages/Fusion")"#),
            "first alias should resolve, got: {source}"
        );
        assert!(
            source.contains(r#"require("./Packages/Roact")"#),
            "second alias should resolve, got: {source}"
        );
        assert!(
            source.contains(r#"require("@self/SubModule")"#),
            "@self should be left untouched, got: {source}"
        );
    }

    #[test]
    fn integration_bare_alias() {
        let session = create_test_session(vec![(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "test",
                            "tree": {
                                "$className": "Folder",
                                "Lib": {
                                    "$path": "lib"
                                },
                                "Source": {
                                    "$path": "src"
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases": {"lib": "lib"}}"#),
                ),
                (
                    "lib",
                    VfsSnapshot::dir([("init.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "Main.luau",
                        VfsSnapshot::file(
                            r#"local Lib = require("@lib")
return {}"#,
                        ),
                    )]),
                ),
            ]),
        )]);

        let source = find_source_by_name(&session, "Main").expect("should find Main script");
        // Main at src/, lib at lib/
        // Relative: ../lib
        assert!(
            source.contains(r#"require("../Lib")"#),
            "bare alias should resolve to relative path, got: {source}"
        );
    }

    #[test]
    fn no_double_slash_for_sibling_in_alias_directory() {
        // When a script is inside the alias target directory and
        // requires a sibling via the alias, the path should not
        // contain double slashes.
        let result = compute_relative_require(
            Path::new("/project/Stories"),
            Path::new("/project/Stories"),
            Some("makeStory"),
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "./makeStory");
        assert!(
            !result.contains("//"),
            "should not have double slashes, got: {result}"
        );
    }

    #[test]
    fn same_directory_no_sub_path() {
        let result = compute_relative_require(
            Path::new("/project/Stories"),
            Path::new("/project/Stories"),
            None,
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, ".");
    }

    #[test]
    fn child_directory_with_sub_path() {
        let result = compute_relative_require(
            Path::new("/project/Source"),
            Path::new("/project/Source/Lib"),
            Some("Util"),
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "./Lib/Util");
    }

    #[test]
    fn parent_directory_with_sub_path() {
        let result = compute_relative_require(
            Path::new("/project/Source/Lib"),
            Path::new("/project/Source"),
            Some("Main"),
            &[],
            Path::new("/project"),
            false,
        );
        assert_eq!(result, "../Main");
    }
}
