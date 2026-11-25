use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::Context as _;
use full_moon::{
    ast::{self, Expression, Prefix, Suffix},
    tokenizer::{Token, TokenReference, TokenType},
};
use memofs::Vfs;
use serde::Deserialize;

const REQUIRE_CALL_CACHE_BYTES: usize = 64 * 1024 * 1024;
const REQUIRE_CALL_CACHE_ENTRIES: usize = 16384;

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
    /// Project tree `$path` mappings for DataModel-aware path computation. Collected
    /// mounts are normalized absolute paths; relative inputs resolve from project_root.
    pub path_mappings: &'a [(PathBuf, Vec<String>)],
    /// The project root directory.
    pub project_root: &'a Path,
}

/// A refresh resolves many imports against one immutable mapping list. Remember the
/// original ordinal so indexed lookup preserves first-match semantics, even for
/// unsorted mappings or a script's extensionless alias colliding with another mount.
pub(super) struct PathMappingIndex<'a> {
    mappings: &'a [(PathBuf, Vec<String>)],
    prefixes: HashMap<Cow<'a, Path>, usize>,
}

impl<'a> PathMappingIndex<'a> {
    pub(super) fn new(mappings: &'a [(PathBuf, Vec<String>)], root: &Path) -> Self {
        profiling::scope!(
            "Rojo.BuildImportMappingIndex",
            format!("Mappings {}", mappings.len()).as_str()
        );
        let mut prefixes = HashMap::with_capacity(mappings.len());
        for (ordinal, (path, _)) in mappings.iter().enumerate() {
            let normalized = crate::package_sources::normalize(&root.join(path));
            let prefix = if normalized == *path {
                Cow::Borrowed(path.as_path())
            } else {
                Cow::Owned(normalized)
            };
            let extensionless = prefix
                .extension()
                .filter(|extension| *extension == "lua" || *extension == "luau")
                .map(|_| prefix.with_extension(""));
            prefixes.entry(prefix).or_insert(ordinal);
            if let Some(extensionless) = extensionless {
                prefixes.entry(Cow::Owned(extensionless)).or_insert(ordinal);
            }
        }
        Self { mappings, prefixes }
    }

    fn matching_prefix<'p>(&self, relative: &'p Path) -> (Option<(usize, &'p Path)>, usize) {
        let mut selected = None;
        let mut candidates = 0;
        for prefix in relative.ancestors().chain(std::iter::once(Path::new(""))) {
            candidates += 1;
            if let Some(&ordinal) = self.prefixes.get(prefix) {
                if selected.is_none_or(|(previous, _)| ordinal < previous) {
                    selected = Some((ordinal, prefix));
                }
            }
        }
        (selected, candidates)
    }

    fn mapped(&self, fs_path: &Path, root: &Path) -> Option<Vec<String>> {
        let normalized = crate::package_sources::normalize(&root.join(fs_path));
        let (ordinal, prefix) = self.matching_prefix(&normalized).0?;
        let mut result = self.mappings[ordinal].1.clone();
        result.extend(
            normalized
                .strip_prefix(prefix)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned()),
        );
        Some(result)
    }

    fn resolve(&self, fs_path: &Path, root: &Path) -> Vec<String> {
        self.mapped(fs_path, root)
            .unwrap_or_else(|| filesystem_path_segments(fs_path, root))
    }
}

#[derive(Clone, Copy)]
struct PathMappings<'a> {
    paths: &'a [(PathBuf, Vec<String>)],
    root: &'a Path,
    index: Option<&'a PathMappingIndex<'a>>,
}

impl PathMappings<'_> {
    fn mapped(self, path: &Path) -> Option<Vec<String>> {
        match self.index {
            Some(index) => index.mapped(path, self.root),
            None => mapped_datamodel_path(path, self.paths, self.root),
        }
    }

    fn resolve(self, path: &Path) -> Vec<String> {
        match self.index {
            Some(index) => index.resolve(path, self.root),
            None => to_datamodel_path(path, self.paths, self.root),
        }
    }
}

/// Resolve `.luaurc` alias-based requires in the given Luau source
/// code. Transforms `require("@alias/path")` into relative
/// require-by-string paths like `require("../../Packages/path")`.
///
/// Returns the transformed source, or the original if no changes were
/// needed.
pub fn resolve_requires(source: &str, info: &ResolveInfo) -> anyhow::Result<String> {
    resolve_requires_in_context(source, info, false)
}

pub fn resolve_requires_in_context(
    source: &str,
    info: &ResolveInfo,
    datamodel: bool,
) -> anyhow::Result<String> {
    resolve_requires_with_package_dependencies(source, info, datamodel, None)
}

pub(crate) fn resolve_requires_with_package_dependencies(
    source: &str,
    info: &ResolveInfo,
    datamodel: bool,
    package_dependencies: Option<&Path>,
) -> anyhow::Result<String> {
    resolve_requires_with_mapping_index(source, info, datamodel, package_dependencies, None)
}

pub(super) fn resolve_requires_with_mapping_index(
    source: &str,
    info: &ResolveInfo,
    datamodel: bool,
    package_dependencies: Option<&Path>,
    index: Option<&PathMappingIndex>,
) -> anyhow::Result<String> {
    profiling::scope!(
        "Rojo.ResolveSourceImports",
        format!(
            "Input bytes {}, mappings {}, indexed {}",
            source.len(),
            info.path_mappings.len(),
            index.is_some()
        )
        .as_str()
    );
    let mappings = PathMappings {
        paths: info.path_mappings,
        root: info.project_root,
        index,
    };
    let file_dir = info
        .file_path
        .parent()
        .context("script file has no parent directory")?;

    let luaurc = find_luaurc(info.vfs, file_dir)?;

    let mut aliases = luaurc
        .as_ref()
        .map(|(aliases, _)| aliases.clone())
        .unwrap_or_default();
    if let Some(directory) = package_dependencies {
        // Package ownership changes only this namespace. Other authored aliases
        // still resolve from the original source configuration, not its stale copy.
        aliases.retain(|name, _| !name.eq_ignore_ascii_case("packages"));
        aliases.insert(
            "packages".into(),
            directory
                .to_str()
                .context("Non-UTF8 package dependency directory")?
                .to_owned(),
        );
    }

    if aliases.is_empty() {
        return Ok(source.to_string());
    }

    let luaurc_dir = luaurc
        .as_ref()
        .map(|(_, directory)| directory.as_path())
        .unwrap_or(info.project_root);

    // Quick check: skip parsing if no custom alias strings present
    // (alias names match without regard to case, so the check does too)
    let lowercase_source = source.to_ascii_lowercase();
    if !aliases
        .keys()
        .any(|k| lowercase_source.contains(&format!("@{}", k.to_ascii_lowercase())))
    {
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

    let replacements = find_require_replacements_with_mappings(
        source, file_dir, &aliases, luaurc_dir, mappings, is_init,
    )?;

    if replacements.is_empty() {
        return Ok(source.to_string());
    }

    // Apply replacements in reverse order to preserve byte offsets
    let mut result = source.to_string();
    let mut sorted = replacements;
    sorted.sort_by(|a, b| b.start.cmp(&a.start));

    for mut replacement in sorted {
        if datamodel {
            let canonical =
                canonicalize_alias_target(info.vfs, &replacement.target).map_err(|error| {
                    anyhow::Error::new(UnmappedDataModelImport {
                        target: replacement.target.clone(),
                    })
                    .context(error)
                })?;
            let target = mappings
                .mapped(&canonical)
                .ok_or_else(|| UnmappedDataModelImport {
                    target: replacement.target.clone(),
                })?;
            replacement.new_text = format!("require(\"@game/{}\")", target.join("/"));
        }
        result.replace_range(replacement.start..replacement.end, &replacement.new_text);
    }

    Ok(result)
}

// Extensionless module names are not filesystem entries. Resolve their script file or
// closest existing ancestor so symlinks use the same spelling as collected mount paths.
fn canonicalize_alias_target(vfs: &Vfs, target: &Path) -> std::io::Result<PathBuf> {
    for suffix in ["", ".luau", ".lua"] {
        let mut candidate = target.as_os_str().to_owned();
        candidate.push(suffix);
        match vfs.canonicalize(Path::new(&candidate)) {
            Ok(canonical) => {
                return Ok(if suffix.is_empty() {
                    canonical
                } else {
                    canonical.with_extension("")
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for ancestor in target.ancestors().skip(1) {
        match vfs.canonicalize(ancestor) {
            Ok(canonical) => return Ok(canonical.join(target.strip_prefix(ancestor).unwrap())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(target.to_path_buf())
}

#[derive(Debug, thiserror::Error)]
#[error("Source import inversion is deferred until the draft can be serialized safely")]
pub(crate) struct SourceSerializationDeferred;

/// Invert only proven token correspondences. Changed import topology is not a
/// license to guess an alias or leak a generated import into source.
pub(crate) fn restore_source(
    transform: &crate::snapshot::SourceTransform,
    edited: &str,
) -> anyhow::Result<String> {
    profiling::scope!(
        "Rojo.RestoreSourceImports",
        format!(
            "Original bytes {}, emitted bytes {}, edited bytes {}",
            transform.original.len(),
            transform.emitted.len(),
            edited.len()
        )
        .as_str()
    );
    if edited == transform.emitted {
        return Ok(transform.original.clone());
    }
    fn calls(source: &str) -> anyhow::Result<Vec<(Range<usize>, String)>> {
        profiling::scope!(
            "Rojo.ParseSourceImports",
            format!("Input bytes {}", source.len()).as_str()
        );
        let ast = full_moon::parse(source).map_err(|e| {
            anyhow::anyhow!("Cannot safely reverse imports in unparsable source {e:?}")
        })?;
        let mut result = Vec::new();
        visit_block(ast.nodes(), &mut |range, content| {
            result.push((range, content))
        });
        result.sort_by_key(|(range, _)| range.start);
        Ok(result)
    }
    let original = calls(&transform.original)?;
    let emitted = calls(&transform.emitted)?;
    let edited_calls = calls(edited)?;
    anyhow::ensure!(
        original.len() == emitted.len() && emitted.len() == edited_calls.len(),
        "Import structure changed. Resolve source imports on the filesystem before syncback"
    );
    let mut result = edited.to_owned();
    let ambiguous = |before: &str, after: &str| {
        original.iter().zip(&emitted).any(|(original, emitted)| {
            emitted.1 == after && &transform.original[original.0.clone()] != before
        })
    };
    for ((original, emitted), edited) in original.iter().zip(&emitted).zip(&edited_calls).rev() {
        if original.1 != emitted.1 {
            anyhow::ensure!(
                edited.1 == emitted.1
                    && result.get(edited.0.clone()) == transform.emitted.get(emitted.0.clone()),
                "Generated import was edited ambiguously. Resolve it in the original source"
            );
            anyhow::ensure!(
                !emitted.1.is_empty()
                    && !ambiguous(&transform.original[original.0.clone()], &emitted.1),
                "Ambiguous emitted import alias"
            );
            result.replace_range(edited.0.clone(), &transform.original[original.0.clone()]);
        }
    }
    Ok(result)
}

/// A text replacement to apply to the source.
struct Replacement {
    start: usize,
    end: usize,
    new_text: String,
    target: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("Alias target \"{}\" is not mounted in the DataModel", .target.display())]
pub(crate) struct UnmappedDataModelImport {
    target: PathBuf,
}

/// Compute the relative require-by-string path from a script's
/// directory to an alias target path, with an optional sub-path.
///
/// When `path_mappings` are available (from the project tree), paths
/// are projected into the DataModel tree before computing the
/// relative path. This accounts for project tree remappings like
/// `"Plugin": { "$path": "Source/Plugin" }` where the filesystem
/// has an extra directory level that doesn't exist in the DataModel.
#[cfg(test)]
fn compute_relative_require(
    script_dir: &Path,
    alias_target: &Path,
    sub_path: Option<&str>,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> String {
    compute_relative_require_with_mappings(
        script_dir,
        alias_target,
        sub_path,
        PathMappings {
            paths: path_mappings,
            root: project_root,
            index: None,
        },
        is_init,
    )
}

fn compute_relative_require_with_mappings(
    script_dir: &Path,
    alias_target: &Path,
    sub_path: Option<&str>,
    mappings: PathMappings,
    is_init: bool,
) -> String {
    let target = sub_path.map_or_else(|| alias_target.to_path_buf(), |sub| alias_target.join(sub));
    if !mappings.paths.is_empty() {
        compute_datamodel_relative(script_dir, &target, mappings, is_init)
    } else {
        compute_filesystem_relative(script_dir, &target)
    }
}

/// Compute the relative path using DataModel tree positions derived
/// from the project tree `$path` mappings.
fn compute_datamodel_relative(
    script_dir: &Path,
    alias_target: &Path,
    mappings: PathMappings,
    is_init: bool,
) -> String {
    let mut script_dm = mappings.resolve(script_dir);
    let target_dm = mappings.resolve(alias_target);

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
    mapped_datamodel_path(fs_path, path_mappings, project_root)
        .unwrap_or_else(|| filesystem_path_segments(fs_path, project_root))
}

fn mapped_datamodel_path(
    fs_path: &Path,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
) -> Option<Vec<String>> {
    let normalized = crate::package_sources::normalize(&project_root.join(fs_path));
    for (mount_fs, mount_dm) in path_mappings {
        let mount_fs = crate::package_sources::normalize(&project_root.join(mount_fs));
        let without_extension = mount_fs
            .extension()
            .filter(|extension| *extension == "lua" || *extension == "luau")
            .map(|_| mount_fs.with_extension(""));
        if let Ok(remainder) = normalized
            .strip_prefix(&mount_fs)
            .or_else(|_| normalized.strip_prefix(without_extension.as_deref().unwrap_or(&mount_fs)))
        {
            let mut dm_path = mount_dm.clone();
            for component in remainder.components() {
                dm_path.push(component.as_os_str().to_string_lossy().into_owned());
            }
            return Some(dm_path);
        }
    }
    None
}

fn filesystem_path_segments(fs_path: &Path, project_root: &Path) -> Vec<String> {
    let normalized = crate::package_sources::normalize(&project_root.join(fs_path));
    let root = crate::package_sources::normalize(project_root);
    normalized
        .strip_prefix(&root)
        .unwrap_or(&normalized)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
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
#[cfg(test)]
fn find_require_replacements(
    source: &str,
    script_dir: &Path,
    aliases: &HashMap<String, String>,
    luaurc_dir: &Path,
    path_mappings: &[(PathBuf, Vec<String>)],
    project_root: &Path,
    is_init: bool,
) -> anyhow::Result<Vec<Replacement>> {
    find_require_replacements_with_mappings(
        source,
        script_dir,
        aliases,
        luaurc_dir,
        PathMappings {
            paths: path_mappings,
            root: project_root,
            index: None,
        },
        is_init,
    )
}

fn find_require_replacements_with_mappings(
    source: &str,
    script_dir: &Path,
    aliases: &HashMap<String, String>,
    luaurc_dir: &Path,
    mappings: PathMappings,
    is_init: bool,
) -> anyhow::Result<Vec<Replacement>> {
    let calls = parse_require_calls(source)?;
    let mut replacements = Vec::new();
    for (range, content) in calls.iter() {
        if !content.starts_with('@') {
            continue;
        }
        if let Some(replacement) = resolve_alias_require(
            (content, range.clone()),
            script_dir,
            aliases,
            luaurc_dir,
            mappings,
            is_init,
        ) {
            replacements.push(replacement);
        }
    }
    Ok(replacements)
}

type RequireCalls = Arc<[(Range<usize>, String)]>;

struct RequireCallCache {
    entries: HashMap<Arc<str>, RequireCalls>,
    order: VecDeque<Arc<str>>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl RequireCallCache {
    fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    fn weight(source: &str, calls: &RequireCalls) -> usize {
        source.len()
            + calls
                .iter()
                .map(|(_, content)| std::mem::size_of::<(Range<usize>, String)>() + content.len())
                .sum::<usize>()
    }

    fn get(&self, source: &str) -> Option<RequireCalls> {
        self.entries.get(source).cloned()
    }

    fn insert(&mut self, source: &str, calls: RequireCalls) -> RequireCalls {
        if let Some(cached) = self.get(source) {
            return cached;
        }
        let weight = Self::weight(source, &calls);
        if weight > self.max_bytes || self.max_entries == 0 {
            return calls;
        }
        while self.bytes + weight > self.max_bytes || self.entries.len() >= self.max_entries {
            let oldest = self.order.pop_front().unwrap();
            let removed = self.entries.remove(&oldest).unwrap();
            self.bytes -= Self::weight(&oldest, &removed);
        }
        let key: Arc<str> = source.into();
        self.bytes += weight;
        self.order.push_back(key.clone());
        self.entries.insert(key, calls.clone());
        calls
    }
}

fn parse_require_calls(source: &str) -> anyhow::Result<RequireCalls> {
    // Only syntax is reusable. Alias files, package ownership, and mappings are
    // still read and resolved on every call, including after structural changes.
    static CACHE: OnceLock<Mutex<RequireCallCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(RequireCallCache::new(
            REQUIRE_CALL_CACHE_BYTES,
            REQUIRE_CALL_CACHE_ENTRIES,
        ))
    });
    if let Some(calls) = cache.lock().unwrap().get(source) {
        return Ok(calls);
    }
    // Parsing outside the lock lets independent startup workers proceed together.
    // Exact source bytes are the key; neither failed parses nor resolved paths live here.
    let ast = full_moon::parse(source)
        .map_err(|e| anyhow::anyhow!("failed to parse Luau source: {e:?}"))?;
    let mut calls = Vec::new();
    visit_block(ast.nodes(), &mut |range, content| {
        calls.push((range, content));
    });
    Ok(cache.lock().unwrap().insert(source, calls.into()))
}

/// Visit a block, looking for require() calls with string arguments.
fn visit_block(block: &ast::Block, callback: &mut dyn FnMut(Range<usize>, String)) {
    for stmt in block.stmts() {
        visit_stmt(stmt, callback);
    }
    if let Some(ast::LastStmt::Return(ret)) = block.last_stmt() {
        for expr in ret.returns() {
            visit_expr(expr, callback);
        }
    }
}

fn visit_stmt(stmt: &ast::Stmt, callback: &mut dyn FnMut(Range<usize>, String)) {
    match stmt {
        ast::Stmt::FunctionCall(call) => {
            visit_call(call, callback);
        }
        ast::Stmt::LocalAssignment(local) => {
            for expr in local.expressions() {
                visit_expr(expr, callback);
            }
        }
        ast::Stmt::ConstAssignment(constant) => {
            for expr in constant.expressions() {
                visit_expr(expr, callback);
            }
        }
        ast::Stmt::Assignment(assign) => {
            for var in assign.variables() {
                visit_var(var, callback);
            }
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
        // Requires inside function bodies, such as lazy requires, need rewriting too
        ast::Stmt::LocalFunction(function) => {
            visit_function_body(function.body(), callback);
        }
        ast::Stmt::FunctionDeclaration(function) => {
            visit_function_body(function.body(), callback);
        }
        ast::Stmt::ConstFunction(function) => {
            visit_function_body(function.body(), callback);
        }
        _ => {}
    }
}

/// Visit a function body, unless one of its parameters is named `require`. Inside such a
/// function, `require` is the caller's value rather than Roblox's module loader, so its
/// calls must keep their original arguments.
fn visit_function_body(body: &ast::FunctionBody, callback: &mut dyn FnMut(Range<usize>, String)) {
    let shadows_require = body.parameters().iter().any(|parameter| {
        matches!(parameter, ast::Parameter::Name(name) if name.token().to_string() == "require")
    });
    if !shadows_require {
        visit_block(body.block(), callback);
    }
}

/// Check a call or index chain such as `require("@x").Field:Method()` for a require at its
/// head, then everything passed or indexed along it, so requires inside callbacks,
/// argument tables, and bracket keys are found too.
fn visit_chain<'a>(
    prefix: &Prefix,
    suffixes: impl Iterator<Item = &'a Suffix>,
    callback: &mut dyn FnMut(Range<usize>, String),
) {
    if let Prefix::Expression(expression) = prefix {
        visit_expr(expression, callback);
    }
    let mut suffixes = suffixes.peekable();
    if let Some(first) = suffixes.peek() {
        check_require_call(prefix, first, callback);
    }
    for suffix in suffixes {
        match suffix {
            Suffix::Call(ast::Call::AnonymousCall(args)) => visit_function_args(args, callback),
            Suffix::Call(ast::Call::MethodCall(method)) => {
                visit_function_args(method.args(), callback)
            }
            Suffix::Index(ast::Index::Brackets { expression, .. }) => {
                visit_expr(expression, callback)
            }
            _ => {}
        }
    }
}

fn visit_call(call: &ast::FunctionCall, callback: &mut dyn FnMut(Range<usize>, String)) {
    visit_chain(call.prefix(), call.suffixes(), callback);
}

fn visit_var(var: &ast::Var, callback: &mut dyn FnMut(Range<usize>, String)) {
    if let ast::Var::Expression(var_expression) = var {
        visit_chain(var_expression.prefix(), var_expression.suffixes(), callback);
    }
}

fn visit_function_args(args: &ast::FunctionArgs, callback: &mut dyn FnMut(Range<usize>, String)) {
    match args {
        ast::FunctionArgs::Parentheses { arguments, .. } => {
            for expr in arguments {
                visit_expr(expr, callback);
            }
        }
        ast::FunctionArgs::TableConstructor(table) => visit_table(table, callback),
        _ => {}
    }
}

fn visit_table(table: &ast::TableConstructor, callback: &mut dyn FnMut(Range<usize>, String)) {
    for field in table.fields() {
        match field {
            ast::Field::ExpressionKey { key, value, .. } => {
                visit_expr(key, callback);
                visit_expr(value, callback);
            }
            ast::Field::NameKey { value, .. } => visit_expr(value, callback),
            ast::Field::NoKey(value) => visit_expr(value, callback),
            _ => {}
        }
    }
}

fn visit_expr(expr: &Expression, callback: &mut dyn FnMut(Range<usize>, String)) {
    match expr {
        Expression::FunctionCall(call) => {
            visit_call(call, callback);
        }
        Expression::Parentheses { expression, .. }
        | Expression::TypeAssertion { expression, .. } => {
            visit_expr(expression, callback);
        }
        Expression::Function(function) => {
            visit_function_body(function.body(), callback);
        }
        Expression::TableConstructor(table) => {
            visit_table(table, callback);
        }
        Expression::BinaryOperator { lhs, rhs, .. } => {
            visit_expr(lhs, callback);
            visit_expr(rhs, callback);
        }
        Expression::UnaryOperator { expression, .. } => {
            visit_expr(expression, callback);
        }
        Expression::Var(var) => {
            visit_var(var, callback);
        }
        _ => {}
    }
}

/// Check if a chain starts with a literal string require and invoke the callback with
/// the byte range of just that call (not anything chained after it, like `.Field`).
/// Include generated relative imports so syncback can match them to authored aliases.
fn check_require_call(
    prefix: &Prefix,
    first_suffix: &Suffix,
    callback: &mut dyn FnMut(Range<usize>, String),
) {
    let Prefix::Name(name) = prefix else {
        return;
    };
    if name.token().to_string() != "require" {
        return;
    }

    let Suffix::Call(ast::Call::AnonymousCall(args)) = first_suffix else {
        return;
    };
    let content = match args.as_ref() {
        ast::FunctionArgs::Parentheses {
            arguments,
            parentheses: _,
        } => {
            let args: Vec<_> = arguments.iter().collect();
            if args.len() != 1 {
                return;
            }
            extract_string_from_expr(args[0])
        }
        ast::FunctionArgs::String(string_token) => extract_string_from_token(string_token),
        _ => return,
    };

    if let Some(content) = content {
        let start = name.token().start_position().bytes();
        callback(start..func_args_byte_end(args), content);
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
    call: (&str, Range<usize>),
    script_dir: &Path,
    aliases: &HashMap<String, String>,
    luaurc_dir: &Path,
    mappings: PathMappings,
    is_init: bool,
) -> Option<Replacement> {
    let (alias_path, call_range) = call;
    let without_at = &alias_path[1..];

    let (alias_name, sub_path) = match without_at.find('/') {
        Some(idx) => (&without_at[..idx], Some(&without_at[idx + 1..])),
        None => (without_at, None),
    };

    // Native Roblox aliases must bypass custom alias lookup, including its unknown-alias
    // warning. A .luaurc entry cannot override their runtime meaning.
    if alias_name.eq_ignore_ascii_case("self") || alias_name.eq_ignore_ascii_case("game") {
        return None;
    }

    // Luau matches alias names without regard to case
    let alias_target = aliases
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(alias_name))
        .map(|(_, target)| target);

    if let Some(alias_target) = alias_target {
        // Aliases into the home folder (like Lune's `~/.lune/.typedefs/`) point outside
        // the project, so they can't become part of the place and are left as they are.
        // A folder that merely starts with `~` (like `~packages`) is still project-relative.
        if alias_target == "~" || alias_target.starts_with("~/") || alias_target.starts_with("~\\")
        {
            return None;
        }

        let alias_fs_path = luaurc_dir.join(alias_target);

        let require_path = compute_relative_require_with_mappings(
            script_dir,
            &alias_fs_path,
            sub_path,
            mappings,
            is_init,
        );

        Some(Replacement {
            start: call_range.start,
            end: call_range.end,
            new_text: format!(r#"require("{require_path}")"#),
            target: sub_path.map_or_else(|| alias_fs_path.clone(), |sub| alias_fs_path.join(sub)),
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
    fn unchanged_require_syntax_reuses_extraction_without_reusing_resolution() {
        let source = "-- Syntax reuse regression.\nreturn require('@shared/Value')";
        let first = parse_require_calls(source).unwrap();
        let second = parse_require_calls(source).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1, "@shared/Value");
        assert!(Arc::ptr_eq(&first, &second));

        let resolve = |target: &str| {
            find_require_replacements(
                source,
                Path::new("/project/Scripts"),
                &[("shared".to_owned(), target.to_owned())].into(),
                Path::new("/project"),
                &[],
                Path::new("/project"),
                false,
            )
            .unwrap()
            .remove(0)
            .new_text
        };
        assert_ne!(resolve("./First"), resolve("./Second"));
    }

    #[test]
    fn require_syntax_reuse_never_accepts_changed_or_invalid_source() {
        let first = parse_require_calls("return require('@one/Value')").unwrap();
        let second = parse_require_calls("return require('@two/Value')").unwrap();
        assert_ne!(first, second);
        for _ in 0..2 {
            assert!(parse_require_calls("return require('@one/Value'").is_err());
        }
    }

    #[test]
    fn require_syntax_cache_obeys_byte_and_entry_bounds() {
        let calls: RequireCalls = vec![(0..4, "@shared/Value".to_owned())].into();
        let entry_bytes = RequireCallCache::weight("first", &calls);
        for (max_bytes, max_entries) in [(entry_bytes, 10), (usize::MAX, 1)] {
            let mut cache = RequireCallCache::new(max_bytes, max_entries);
            cache.insert("first", calls.clone());
            assert_eq!(cache.get("first").as_deref(), Some(calls.as_ref()));
            let retained = cache.bytes;
            cache.insert("first", calls.clone());
            assert_eq!(cache.bytes, retained);
            cache.insert("other", calls.clone());
            assert!(cache.get("first").is_none());
            assert_eq!(cache.get("other").as_deref(), Some(calls.as_ref()));
            assert!(cache.bytes <= max_bytes);
            assert!(cache.entries.len() <= max_entries);
        }
        for (max_bytes, max_entries) in [(0, 10), (usize::MAX, 0)] {
            let mut cache = RequireCallCache::new(max_bytes, max_entries);
            assert_eq!(cache.insert("first", calls.clone()), calls);
            assert!(cache.get("first").is_none());
            assert_eq!(cache.bytes, 0);
        }
    }

    #[test]
    fn indexed_mapping_lookup_matches_linear_first_match_semantics() {
        let entries = [
            ("Source", "Broad"),
            ("Source/Shared", "Nested"),
            ("Source/Shared/Thing.luau", "Script"),
            ("Source/Shared/Thing", "ExtensionlessCollision"),
            ("Source/Shared/Thing.luau", "Duplicate"),
            ("Source/Worker.server.luau", "Worker"),
            ("Source/Legacy.lua", "Legacy"),
            ("", "EmptyPrefix"),
            ("/outside", "Absolute"),
            ("Source/Shared/./Other", "ComponentNormalization"),
        ];
        let queries = [
            "/root",
            "/root/Source",
            "/root/Source/Shared",
            "/root/Source/Shared/Thing",
            "/root/Source/Shared/Thing.luau",
            "/root/Source/Shared/Thing/Child",
            "/root/Source/Shared/Thing.luau/Child",
            "/root/Source/Worker.server",
            "/root/Source/Worker.server.luau",
            "/root/Source/Legacy",
            "/root/Source/Legacy.lua",
            "/root/Source/Shared/Other",
            "/root/Source/Shared/../Shared/Thing",
            "/root/Unmapped/Code",
            "/outside/Code",
            "relative/Unmapped",
        ];
        let root = Path::new("/root");
        for length in 0..=entries.len() {
            let mut mappings = entries[..length]
                .iter()
                .map(|(path, target)| (PathBuf::from(path), vec![target.to_string()]))
                .collect::<Vec<_>>();
            for reversed in [false, true] {
                if reversed {
                    mappings.reverse();
                }
                for _ in 0..length.max(1) {
                    let index = PathMappingIndex::new(&mappings, Path::new("/root"));
                    for query in queries {
                        assert_eq!(
                            index.resolve(Path::new(query), root),
                            to_datamodel_path(Path::new(query), &mappings, root),
                            "query {query}, mappings {mappings:?}"
                        );
                    }
                    if !mappings.is_empty() {
                        mappings.rotate_left(1);
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_external_mappings_match_verbatim_and_compatible_targets() {
        let root = Path::new(r"C:\checkout with spaces\.local\config");
        for mount in [
            r"C:\checkout with spaces\Packages",
            r"\\?\C:\checkout with spaces\Packages",
        ] {
            let mappings = vec![(
                PathBuf::from(mount),
                vec!["ReplicatedStorage".into(), "Packages".into()],
            )];
            let index = PathMappingIndex::new(&mappings, root);
            for query in [
                r"C:\checkout with spaces\Packages\Maid",
                r"\\?\C:\checkout with spaces\Packages\Maid",
            ] {
                let expected = Some(vec![
                    "ReplicatedStorage".into(),
                    "Packages".into(),
                    "Maid".into(),
                ]);
                assert_eq!(index.mapped(Path::new(query), root), expected);
                assert_eq!(
                    mapped_datamodel_path(Path::new(query), &mappings, root),
                    expected
                );
            }
        }
    }

    #[test]
    fn indexed_lookup_work_is_bounded_by_path_depth_not_mapping_count() {
        let root = Path::new("/root");
        let query = root.join("Source/Shared/Target/Child");
        let expected_probes = query.ancestors().count() + 1;
        for count in [0, 10, 10_000] {
            let mut mappings = (0..count)
                .map(|index| {
                    (
                        PathBuf::from(format!("Other/Module{index}.luau")),
                        vec![format!("Module{index}")],
                    )
                })
                .collect::<Vec<_>>();
            mappings.push(("Source/Shared/Target.luau".into(), vec!["Mapped".into()]));
            let index = PathMappingIndex::new(&mappings, Path::new("/root"));
            assert_eq!(index.matching_prefix(&query).1, expected_probes);
            assert_eq!(index.resolve(&query, root), ["Mapped", "Child"]);
        }
    }

    #[test]
    fn indexed_resolution_preserves_import_output_and_reversal() {
        let mut memory = memofs::InMemoryFs::new();
        memory
            .load_snapshot(
                "/root/.luaurc",
                memofs::VfsSnapshot::file(r#"{"aliases":{"shared":"Source/Shared"}}"#),
            )
            .unwrap();
        let vfs = Vfs::new(memory);
        let mappings = vec![
            (
                "Source/Shared/Util.luau".into(),
                vec!["ReplicatedStorage".into(), "RenamedUtil".into()],
            ),
            (
                "Source/Shared".into(),
                vec!["ReplicatedStorage".into(), "Shared".into()],
            ),
            (
                "Source/App".into(),
                vec!["ServerScriptService".into(), "App".into()],
            ),
        ];
        let index = PathMappingIndex::new(&mappings, Path::new("/root"));
        let source = "return require(\"@shared/Util\"), 1\n";
        for datamodel in [false, true] {
            for path in ["/root/Source/App/Code.luau", "/root/Source/App/init.luau"] {
                let info = ResolveInfo {
                    file_path: Path::new(path),
                    vfs: &vfs,
                    path_mappings: &mappings,
                    project_root: Path::new("/root"),
                };
                let emitted = resolve_requires_with_mapping_index(
                    source,
                    &info,
                    datamodel,
                    None,
                    Some(&index),
                )
                .unwrap();
                assert_eq!(
                    emitted,
                    resolve_requires_with_package_dependencies(source, &info, datamodel, None)
                        .unwrap()
                );
                let transform = crate::snapshot::SourceTransform {
                    original: source.into(),
                    emitted: emitted.clone(),
                };
                let edited = emitted.replace(", 1", ", 2");
                assert_eq!(
                    restore_source(&transform, &edited).unwrap(),
                    source.replace(", 1", ", 2")
                );
            }
        }
    }

    #[test]
    fn package_dependency_aliases_work_without_authored_config_in_places_and_models() {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        let mappings = vec![
            (
                PathBuf::from("Source/Foo"),
                vec![
                    "ReplicatedStorage".into(),
                    "Packages".into(),
                    "_Index".into(),
                    "Foo".into(),
                    "foo".into(),
                ],
            ),
            (
                PathBuf::from("Packages/_Index/Foo"),
                vec![
                    "ReplicatedStorage".into(),
                    "Packages".into(),
                    "_Index".into(),
                    "Foo".into(),
                ],
            ),
        ];
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Foo/init.luau"),
            vfs: &vfs,
            path_mappings: &mappings,
            project_root: Path::new("/project"),
        };
        for (datamodel, expected) in [
            (false, "return require(\"./Util\")"),
            (
                true,
                "return require(\"@game/ReplicatedStorage/Packages/_Index/Foo/Util\")",
            ),
        ] {
            assert_eq!(
                resolve_requires_with_package_dependencies(
                    "return require('@PaCkAgEs/Util')",
                    &info,
                    datamodel,
                    Some(Path::new("/project/Packages/_Index/Foo")),
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn relative_import_round_trip_preserves_native_edits_and_rejects_ambiguous_changes() {
        let transform = crate::snapshot::SourceTransform {
            original: "local Lib = require('@packages/Foo')\nlocal Native = require('./Native')\nreturn Lib.Value\n".into(),
            emitted: "local Lib = require(\"../../Packages/Foo\")\nlocal Native = require('./Native')\nreturn Lib.Value\n".into(),
        };
        let edited = transform
            .emitted
            .replace("'./Native'", "'./Other'")
            .replace("Lib.Value", "Lib.OtherValue");
        assert_eq!(
            restore_source(&transform, &edited).unwrap(),
            transform
                .original
                .replace("'./Native'", "'./Other'")
                .replace("Lib.Value", "Lib.OtherValue")
        );
        for edited in [
            transform.emitted.replace("Packages/Foo", "Packages/Bar"),
            transform
                .emitted
                .replace("require(", "require(-- comment\n"),
            format!("{}\nrequire('./Extra')", transform.emitted),
            transform.emitted.replace("require('./Native')", "nil"),
        ] {
            assert!(restore_source(&transform, &edited).is_err());
        }
    }

    #[test]
    fn source_round_trip_restores_only_proven_import_tokens() {
        let transform = crate::snapshot::SourceTransform {
            original: "local Lib = require('@packages/Foo')\nreturn Lib.Value\n".into(),
            emitted:
                "local Lib = require(\"@game/ReplicatedStorage/Packages/Foo\")\nreturn Lib.Value\n"
                    .into(),
        };
        assert_eq!(
            restore_source(&transform, &transform.emitted).unwrap(),
            transform.original
        );
        let edited = transform.emitted.replace("Lib.Value", "Lib.OtherValue");
        assert_eq!(
            restore_source(&transform, &edited).unwrap(),
            transform.original.replace("Lib.Value", "Lib.OtherValue")
        );
        for edited in [
            transform.emitted.replace("Packages/Foo", "Packages/Bar"),
            transform
                .emitted
                .replace("require(", "require(-- edited import comment\n"),
            format!("{}\nrequire(\"@game/Other\")", transform.emitted),
            "invalid Luau (((".into(),
        ] {
            assert!(restore_source(&transform, &edited).is_err());
        }
    }

    #[test]
    fn datamodel_aliases_survive_clone_and_reparent_contexts() {
        use memofs::{InMemoryFs, VfsSnapshot};
        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/project/.luaurc",
            VfsSnapshot::file(r#"{"aliases":{"packages":"Packages"}}"#),
        )
        .unwrap();
        let vfs = Vfs::new(fs);
        let mappings = vec![
            (
                PathBuf::from("Original"),
                vec!["ServerScriptService".into(), "Original".into()],
            ),
            (
                PathBuf::from("Clone"),
                vec!["Workspace".into(), "Cloned".into()],
            ),
            (
                PathBuf::from("Packages"),
                vec!["ReplicatedStorage".into(), "Packages".into()],
            ),
        ];
        for file in [
            "/project/Original/Run.client.luau",
            "/project/Clone/Run.client.luau",
            "/project/Clone/init.luau",
        ] {
            let info = ResolveInfo {
                file_path: Path::new(file),
                vfs: &vfs,
                path_mappings: &mappings,
                project_root: Path::new("/project"),
            };
            assert_eq!(
                resolve_requires_in_context("return require(\"@packages/Foo\")", &info, true)
                    .unwrap(),
                "return require(\"@game/ReplicatedStorage/Packages/Foo\")"
            );
            assert!(!resolve_requires_in_context(
                "return require(\"@packages/Foo\")",
                &info,
                false
            )
            .unwrap()
            .contains("@game"));
        }
    }

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
    fn resolve_alias_written_in_a_different_case() {
        let source = r#"local Fusion = require("@DevPackages/Fusion")"#;
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Plugin/init.plugin.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &make_test_vfs(&[(
                "/project/.luaurc",
                r#"{"aliases": {"devpackages": "DevPackages"}}"#,
            )]),
        };

        let result = resolve_requires(source, &info).unwrap();
        assert_eq!(
            result,
            r#"local Fusion = require("../../DevPackages/Fusion")"#
        );
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
    fn mixed_alias_imports_preserve_empty_unicode_and_nonalias_requires() {
        let vfs = make_test_vfs(&[("/project/.luaurc", r#"{"aliases":{"packages":"Packages"}}"#)]);
        let info = ResolveInfo {
            file_path: Path::new("/project/Source/Plugin/init.plugin.luau"),
            path_mappings: &[],
            project_root: Path::new("/project"),
            vfs: &vfs,
        };
        let index = PathMappingIndex::new(info.path_mappings, info.project_root);
        for ordinary in [
            "",
            "é",
            "./Foo",
            "../Foo",
            "xpackages/Fusion",
            "packages/Fusion",
        ] {
            let literal = serde_json::to_string(ordinary).unwrap();
            for call in [format!("require({literal})"), format!("require {literal}")] {
                let source =
                    format!(r#"local Alias = require("@packages/Fusion"); local Other = {call}"#);
                let expected = source.replace(
                    r#"require("@packages/Fusion")"#,
                    r#"require("../../Packages/Fusion")"#,
                );
                for mappings in [None, Some(&index)] {
                    let actual =
                        resolve_requires_with_mapping_index(&source, &info, false, None, mappings)
                            .unwrap();
                    assert_eq!(actual, expected, "{call}");
                }
            }
        }
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
    fn integration_private_projects_map_external_runtime_imports() {
        for absolute in [false, true] {
            for initializer in [false, true] {
                let root = Path::new("/checkout with spaces");
                let private = root.join(".local/config");
                let mounted = |name: &str| {
                    if absolute {
                        root.join(name).to_string_lossy().into_owned()
                    } else {
                        format!("../../{name}")
                    }
                };
                let project = serde_json::json!({
                    "name": "private-place",
                    "tree": {
                        "$className": "DataModel",
                        "ReplicatedStorage": {
                            "$className": "ReplicatedStorage",
                            "Packages": { "$path": mounted("Packages") }
                        },
                        "ServerScriptService": { "$path": mounted("Source") }
                    }
                });
                let consumer = VfsSnapshot::file("return require('@packages/Maid')");
                let source = if initializer {
                    VfsSnapshot::dir([("Consumer", VfsSnapshot::dir([("init.luau", consumer)]))])
                } else {
                    VfsSnapshot::dir([("Consumer.luau", consumer)])
                };
                let mut memory = InMemoryFs::new();
                memory
                    .load_snapshot(
                        root,
                        VfsSnapshot::dir([
                            (
                                ".luaurc",
                                VfsSnapshot::file(r#"{"aliases":{"packages":"Packages"}}"#),
                            ),
                            (
                                ".local",
                                VfsSnapshot::dir([(
                                    "config",
                                    VfsSnapshot::dir([(
                                        "default.project.json",
                                        VfsSnapshot::file(project.to_string()),
                                    )]),
                                )]),
                            ),
                            (
                                "Packages",
                                VfsSnapshot::dir([("Maid.luau", VfsSnapshot::file("return {}"))]),
                            ),
                            ("Source", source),
                        ]),
                    )
                    .unwrap();
                let session = ServeSession::new(Vfs::new(memory), &private).unwrap();
                let source = find_source_by_name(&session, "Consumer").unwrap();
                assert_eq!(
                    source, r#"return require("@game/ReplicatedStorage/Packages/Maid")"#,
                    "absolute {absolute}, initializer {initializer}"
                );
                let require_context = crate::serve_session::build_require_context_from_tree(
                    &session.tree(),
                    &private,
                )
                .unwrap();
                let mut context = crate::snapshot::InstanceContext::default();
                context.require_context = Some(require_context);
                let path = root.join(if initializer {
                    "Source/Consumer"
                } else {
                    "Source/Consumer.luau"
                });
                let live = super::super::snapshot_from_vfs(&context, session.vfs(), &path)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    live.properties.get(&rbx_dom_weak::ustr("Source")),
                    Some(&rbx_dom_weak::types::Variant::String(source))
                );
                assert_eq!(
                    live.metadata.source_transform.unwrap().original,
                    "return require('@packages/Maid')"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_external_mounts_resolve_extensionless_aliases() {
        use std::{fs, os::unix::fs::symlink};

        for (module, link_file) in [
            ("Maid.luau", false),
            ("Maid.lua", false),
            ("Maid/init.luau", false),
            ("Maid.luau", true),
            ("Maid.lua", true),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("checkout with spaces");
            let external = temporary.path().join("external packages");
            fs::create_dir_all(root.join("Source")).unwrap();
            fs::create_dir_all(external.join(module).parent().unwrap()).unwrap();
            fs::write(external.join(module), "return {}").unwrap();
            if link_file {
                fs::create_dir(root.join("Packages")).unwrap();
                symlink(external.join(module), root.join("Packages").join(module)).unwrap();
            } else {
                symlink(&external, root.join("Packages")).unwrap();
            }
            fs::create_dir(root.join("Unmapped")).unwrap();
            fs::write(root.join("Unmapped/Maid.luau"), "return {}").unwrap();
            fs::write(
                root.join(".luaurc"),
                r#"{"aliases":{"packages":"Packages","unmounted":"Unmapped"}}"#,
            )
            .unwrap();
            let original = "return require('@packages/Maid')";
            let consumer = root.join("Source/Consumer.luau");
            fs::write(&consumer, original).unwrap();
            fs::write(
                root.join("default.project.json"),
                r#"{"name":"test","tree":{"$className":"DataModel","ReplicatedStorage":{"$className":"ReplicatedStorage","Packages":{"$path":"Packages"}},"ServerScriptService":{"$path":"Source"}}}"#,
            )
            .unwrap();
            let session =
                ServeSession::new(Vfs::new(memofs::StdBackend::new().unwrap()), &root).unwrap();
            let expected = r#"return require("@game/ReplicatedStorage/Packages/Maid")"#;
            assert_eq!(find_source_by_name(&session, "Consumer").unwrap(), expected);
            let context =
                crate::serve_session::build_require_context_from_tree(&session.tree(), &root)
                    .unwrap();
            let consumer = session.vfs().canonicalize(consumer).unwrap();
            let info = ResolveInfo {
                file_path: &consumer,
                vfs: session.vfs(),
                path_mappings: &context.path_mappings,
                project_root: &root,
            };
            assert_eq!(
                resolve_requires_in_context(original, &info, true).unwrap(),
                expected
            );
            let mut live_context = crate::snapshot::InstanceContext::default();
            live_context.require_context = Some(context.clone());
            let live = super::super::snapshot_from_vfs(&live_context, session.vfs(), &consumer)
                .unwrap()
                .unwrap();
            assert_eq!(
                live.properties.get(&ustr("Source")),
                Some(&Variant::String(expected.into()))
            );
            assert_eq!(live.metadata.source_transform.unwrap().original, original);
            let error =
                resolve_requires_in_context("return require('@unmounted/Maid')", &info, true)
                    .unwrap_err();
            assert!(error.is::<UnmappedDataModelImport>());
        }
    }

    #[test]
    fn datamodel_aliases_without_runtime_mounts_fail_instead_of_emitting_filesystem_paths() {
        let root = Path::new("/checkout");
        let vfs = make_test_vfs(&[(
            "/checkout/.luaurc",
            r#"{"aliases":{"unmounted":"Unmapped"}}"#,
        )]);
        let info = ResolveInfo {
            file_path: Path::new("/checkout/Source/Consumer.luau"),
            vfs: &vfs,
            path_mappings: &[],
            project_root: root,
        };
        let error = resolve_requires_in_context("return require('@unmounted/Value')", &info, true)
            .unwrap_err();
        assert!(error.to_string().contains("DataModel"));
    }

    #[test]
    fn unmapped_datamodel_aliases_fail_initial_and_live_snapshots() {
        use crate::snapshot::{InstanceContext, RequireContext};
        use std::sync::Arc;

        let root = Path::new("/checkout");
        let path = root.join("Source/Consumer.luau");
        let mut memory = InMemoryFs::new();
        memory
            .load_snapshot(
                root,
                VfsSnapshot::dir([
                    (
                        "default.project.json",
                        VfsSnapshot::file(
                            r#"{"name":"test","tree":{"$className":"DataModel","ServerScriptService":{"$path":"Source"}}}"#,
                        ),
                    ),
                    (
                        ".luaurc",
                        VfsSnapshot::file(r#"{"aliases":{"unmounted":"Unmapped"}}"#),
                    ),
                    (
                        "Source",
                        VfsSnapshot::dir([(
                            "Consumer.luau",
                            VfsSnapshot::file("return require('@unmounted/Value')"),
                        )]),
                    ),
                ]),
            )
            .unwrap();
        let vfs = Vfs::new(memory.clone());
        let error = match ServeSession::new(Vfs::new(memory), root) {
            Ok(_) => panic!("An unmapped DataModel import must fail the initial build"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("not mounted in the DataModel"));
        let mut context = InstanceContext::default();
        context.require_context = Some(Arc::new(RequireContext {
            datamodel: true,
            path_mappings: vec![(root.join("Source"), vec!["ServerScriptService".into()])],
            project_root: root.to_path_buf(),
        }));
        let error = super::super::lua::snapshot_lua(
            &context,
            &vfs,
            &path,
            "Consumer",
            super::super::lua::ScriptType::Module,
        )
        .unwrap_err();
        assert!(error.is::<UnmappedDataModelImport>());
        assert!(format!("{error:#}").contains("Consumer.luau"));
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
        // Absolute DataModel imports survive cloning or reparenting the caller.
        assert!(
            source.contains(r#"require("@game/ReplicatedStorage/Packages/SharedLib")"#),
            "Cross-service alias should remain anchored to the DataModel, got {source}"
        );
    }

    #[test]
    fn integration_typed_server_require_is_lowered_and_restored() {
        let original = "local Maid = require(\"@packages/Maid\") :: any\nlocal Native = require(\"@game/ReplicatedStorage/Native\") :: any\nreturn Maid\n";
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
                                    "Packages": { "$path": "Packages" }
                                },
                                "ServerScriptService": {
                                    "$className": "ServerScriptService",
                                    "GameSystems": { "$path": "src" }
                                }
                            }
                        }"#,
                    ),
                ),
                (
                    ".luaurc",
                    VfsSnapshot::file(r#"{"aliases":{"packages":"Packages"}}"#),
                ),
                (
                    "Packages",
                    VfsSnapshot::dir([("Maid.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "ScavengerHuntSystem",
                        VfsSnapshot::dir([(
                            "ServerDuckiez.server.luau",
                            VfsSnapshot::file(original),
                        )]),
                    )]),
                ),
            ]),
        )]);
        let emitted = find_source_by_name(&session, "ServerDuckiez").unwrap();
        assert_eq!(
            emitted,
            original.replace("@packages/Maid", "@game/ReplicatedStorage/Packages/Maid")
        );
        let transform = crate::snapshot::SourceTransform {
            original: original.into(),
            emitted: emitted.clone(),
        };
        assert_eq!(
            restore_source(&transform, &emitted.replace("return Maid", "return Native")).unwrap(),
            original.replace("return Maid", "return Native")
        );
        assert!(restore_source(
            &transform,
            &emitted.replace("Packages/Maid", "Packages/Other")
        )
        .is_err());
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

    /// Returns the replacement text for every alias require found in `source`, for a
    /// script in /project/Source with `@packages` pointing at /project/Packages.
    fn replaced_requires(source: &str) -> Vec<String> {
        let aliases = HashMap::from([("packages".to_string(), "Packages".to_string())]);
        find_require_replacements(
            source,
            Path::new("/project/Source"),
            &aliases,
            Path::new("/project"),
            &[],
            Path::new("/project"),
            false,
        )
        .unwrap()
        .into_iter()
        .map(|replacement| replacement.new_text)
        .collect()
    }

    #[test]
    fn leaves_native_aliases_untouched_even_when_configured() {
        let aliases = HashMap::from([
            ("game".to_string(), "NotTheDataModel".to_string()),
            ("self".to_string(), "NotTheScript".to_string()),
            ("packages".to_string(), "Packages".to_string()),
        ]);
        for alias in ["game", "Game", "GAME", "self", "Self", "SELF"] {
            let source = format!("return require(\"@{alias}/Module\")");
            let replacements = find_require_replacements(
                &source,
                Path::new("/project/Source"),
                &aliases,
                Path::new("/project"),
                &[],
                Path::new("/project"),
                false,
            )
            .unwrap();
            assert!(
                replacements.is_empty(),
                "Native alias @{alias} was rewritten"
            );
        }
        assert_eq!(
            rewritten_source("local a = require(\"@game/ReplicatedStorage/Module\")\nlocal b = require(\"@packages/Maid\")\n"),
            "local a = require(\"@game/ReplicatedStorage/Module\")\nlocal b = require(\"../Packages/Maid\")\n"
        );
    }

    #[test]
    fn rewrites_requires_inside_type_assertions() {
        let source = [
            "local Maid = require(\"@packages/Maid\") :: any",
            "const Other = (require(\"@packages/Other\") :: any) :: unknown",
            "local function load() return require(\"@packages/Lazy\") :: any end",
            "return { Value = require(\"@packages/Value\").Field :: any }",
        ]
        .join("\n");
        let expected = source.replace("@packages/", "../Packages/");
        assert_eq!(rewritten_source(&source), expected);
        let untouched = "local x = loader.require(\"@packages/Maid\") :: any\nlocal function f(require) return require(\"@packages/Maid\") :: any end\n";
        assert_eq!(rewritten_source(untouched), untouched);
    }

    #[test]
    fn rewrites_requires_in_const_declarations() {
        let source = "const t = require(\"@packages/t\")\nconst a, b = 1, require(\"@packages/b\")\nreturn t\n";
        assert_eq!(
            replaced_requires(source),
            vec![
                r#"require("../Packages/t")"#.to_string(),
                r#"require("../Packages/b")"#.to_string(),
            ]
        );
    }

    #[test]
    fn rewrites_requires_inside_function_bodies() {
        let source = [
            "local function a() return require(\"@packages/a\") end",
            "function M.b() return require(\"@packages/b\") end",
            "const function c() return require(\"@packages/c\") end",
            "local d = function() return require(\"@packages/d\") end",
            "return M",
        ]
        .join("\n");
        assert_eq!(
            replaced_requires(&source),
            ["a", "b", "c", "d"]
                .map(|name| format!(r#"require("../Packages/{name}")"#))
                .to_vec()
        );
    }

    #[test]
    fn leaves_calls_to_a_require_parameter_alone() {
        let source = [
            "local function a(require) return require(\"@packages/a\") end",
            "const f = function(x, require) return require(\"@packages/b\") end",
            "local function c() return require(\"@packages/c\") end",
            "return a",
        ]
        .join("\n");
        assert_eq!(
            replaced_requires(&source),
            vec![r#"require("../Packages/c")"#.to_string()]
        );
    }

    #[test]
    fn rewrites_requires_inside_callbacks_and_tables() {
        let source = [
            "task.defer(function() return require(\"@packages/a\") end)",
            "signal:Connect(function() return require(\"@packages/b\") end)",
            "local deps = { c = require(\"@packages/c\"), require(\"@packages/d\") }",
            "local e = setmetatable({}, { __index = function() return require(\"@packages/e\") end })",
            "local f = fallback or require(\"@packages/f\")",
            "return deps",
        ]
        .join("\n");
        assert_eq!(
            replaced_requires(&source),
            ["a", "b", "c", "d", "e", "f"]
                .map(|name| format!(r#"require("../Packages/{name}")"#))
                .to_vec()
        );
    }

    #[test]
    fn matches_alias_names_without_regard_to_case() {
        assert_eq!(
            replaced_requires("local a = require(\"@Packages/a\")\nlocal b = require(\"@PACKAGES/b\")\nreturn a\n"),
            vec![
                r#"require("../Packages/a")"#.to_string(),
                r#"require("../Packages/b")"#.to_string(),
            ]
        );
        assert!(replaced_requires("local m = require(\"@Self/Module\")\nreturn m\n").is_empty());
    }

    #[test]
    fn rewrites_project_folders_that_start_with_a_tilde() {
        let aliases = HashMap::from([("pkgs".to_string(), "~packages".to_string())]);
        let replacements = find_require_replacements(
            "local t = require(\"@pkgs/t\")\nreturn t\n",
            Path::new("/project/Source"),
            &aliases,
            Path::new("/project"),
            &[],
            Path::new("/project"),
            false,
        )
        .unwrap();
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].new_text, r#"require("../~packages/t")"#);
    }

    #[test]
    fn leaves_home_folder_aliases_alone() {
        let aliases =
            HashMap::from([("lune".to_string(), "~/.lune/.typedefs/0.11.1/".to_string())]);
        let replacements = find_require_replacements(
            "local fs = require(\"@lune/fs\")\nreturn fs\n",
            Path::new("/project/Source"),
            &aliases,
            Path::new("/project"),
            &[],
            Path::new("/project"),
            false,
        )
        .unwrap();
        assert!(replacements.is_empty());
    }

    /// Returns the source with every alias require rewritten, for the same setup as
    /// `replaced_requires`.
    fn rewritten_source(source: &str) -> String {
        let aliases = HashMap::from([("packages".to_string(), "Packages".to_string())]);
        let mut replacements = find_require_replacements(
            source,
            Path::new("/project/Source"),
            &aliases,
            Path::new("/project"),
            &[],
            Path::new("/project"),
            false,
        )
        .unwrap();
        replacements.sort_by(|a, b| b.start.cmp(&a.start));
        let mut result = source.to_string();
        for replacement in replacements {
            result.replace_range(replacement.start..replacement.end, &replacement.new_text);
        }
        result
    }

    #[test]
    fn rewrites_requires_followed_by_a_dot_index() {
        assert_eq!(
            rewritten_source("const Command = require(\"@packages/Commandline\").Command\n"),
            "const Command = require(\"../Packages/Commandline\").Command\n"
        );
        assert_eq!(
            rewritten_source("local A = require(\"@packages/a\").B.C\n"),
            "local A = require(\"../Packages/a\").B.C\n"
        );
    }

    #[test]
    fn rewrites_requires_followed_by_a_method_call() {
        assert_eq!(
            rewritten_source("local x = require(\"@packages/a\"):Get(require(\"@packages/b\"))\nrequire(\"@packages/c\"):Start()\n"),
            "local x = require(\"../Packages/a\"):Get(require(\"../Packages/b\"))\nrequire(\"../Packages/c\"):Start()\n"
        );
    }

    #[test]
    fn rewrites_requires_followed_by_a_bracket_index() {
        assert_eq!(
            rewritten_source("local x = require(\"@packages/a\")[\"Key\"]\nlocal y = require(\"@packages/b\")[require(\"@packages/c\")]\n"),
            "local x = require(\"../Packages/a\")[\"Key\"]\nlocal y = require(\"../Packages/b\")[require(\"../Packages/c\")]\n"
        );
    }

    #[test]
    fn rewrites_chained_requires_inside_function_bodies() {
        assert_eq!(
            rewritten_source("local function f()\n\treturn require(\"@packages/a\").Field, require \"@packages/b\".Other\nend\n"),
            "local function f()\n\treturn require(\"../Packages/a\").Field, require(\"../Packages/b\").Other\nend\n"
        );
    }

    #[test]
    fn rewrites_chained_requires_in_assignments_and_calls() {
        assert_eq!(
            rewritten_source("require(\"@packages/a\").Value = require(\"@packages/b\").Default\nrequire(\"@packages/c\").init()\n"),
            "require(\"../Packages/a\").Value = require(\"../Packages/b\").Default\nrequire(\"../Packages/c\").init()\n"
        );
    }

    #[test]
    fn leaves_chained_calls_to_a_require_parameter_alone() {
        let source = "local function f(require)\n\treturn require(\"@packages/a\").Field\nend\n";
        assert_eq!(rewritten_source(source), source);
    }

    #[test]
    fn leaves_requires_that_are_not_at_the_head_of_a_chain_alone() {
        let source = "local x = loader.require(\"@packages/a\").Field\n";
        assert_eq!(rewritten_source(source), source);
    }

    #[test]
    fn rewrites_local_and_const_requires_alike() {
        assert_eq!(
            replaced_requires("local t = require(\"@packages/t\")\nreturn t\n"),
            replaced_requires("const t = require(\"@packages/t\")\nreturn t\n"),
        );
    }
}
