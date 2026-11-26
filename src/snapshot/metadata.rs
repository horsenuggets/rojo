use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{
    glob::{Glob, IgnorableGlob},
    path_serializer,
    project::ProjectNode,
    snapshot_middleware::{emit_legacy_scripts_default, Middleware},
    RojoRef,
};

/// Rojo-specific metadata that can be associated with an instance or a snapshot
/// of an instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceMetadata {
    /// Whether instances not present in the source should be ignored when
    /// live-syncing. This is useful when there are instances that Rojo does not
    /// manage.
    pub ignore_unknown_instances: bool,

    /// If a change occurs to this instance, the instigating source is what
    /// should be run through the snapshot functions to regenerate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instigating_source: Option<InstigatingSource>,

    /// The paths that, when changed, could cause the function that generated
    /// this snapshot to generate a different snapshot. Paths should be included
    /// even if they don't exist, since the presence of a file can change the
    /// outcome of a snapshot function.
    ///
    /// For example, a file named foo.lua might have these relevant paths:
    /// - foo.lua
    /// - foo.meta.json (even if this file doesn't exist!)
    ///
    /// A directory named bar/ might have these:
    /// - bar/
    /// - bar/init.meta.json
    /// - bar/init.lua
    /// - bar/init.server.lua
    /// - bar/init.client.lua
    /// - bar/default.project.json
    ///
    /// This path is used to make sure that file changes update all instances
    /// that may need updates.
    // TODO: Change this to be a SmallVec for performance in common cases?
    #[serde(serialize_with = "path_serializer::serialize_vec_absolute")]
    pub relevant_paths: Vec<PathBuf>,

    /// Contains information about this instance that should persist between
    /// snapshot invocations and is generally inherited.
    ///
    /// If an instance has a piece of context attached to it, then the next time
    /// that instance's instigating source is snapshotted directly, the same
    /// context will be passed into it.
    pub context: InstanceContext,

    /// Indicates the ID used for Ref properties pointing to this Instance.
    pub specified_id: Option<RojoRef>,

    /// The Middleware that was used to create this Instance. Should generally
    /// not be `None` except if the snapshotting process is not completed.
    pub middleware: Option<Middleware>,

    /// A schema provided via a JSON file, if one exists. Will be `None` for
    /// all non-JSON middleware.
    pub schema: Option<String>,

    /// Physical source and emitted source remain distinct across syncback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_transform: Option<SourceTransform>,

    /// Alternate installed paths point at this same logical instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed_paths: Vec<PathBuf>,

    /// Server-only expected bytes protect model files and metadata as well as Source.
    #[serde(skip)]
    pub(crate) package_file_contents: std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,

    /// Exact server-read import inputs, including absent configuration probes. These
    /// describe interpretation, not Source payload, and can never come from a client.
    #[serde(skip)]
    pub(crate) import_inputs: Option<Arc<ImportInputs>>,
}

pub(crate) type ImportInputs = std::collections::BTreeMap<PathBuf, Option<Arc<Vec<u8>>>>;

impl InstanceMetadata {
    pub fn new() -> Self {
        Self {
            ignore_unknown_instances: false,
            instigating_source: None,
            relevant_paths: Vec::new(),
            context: InstanceContext::default(),
            specified_id: None,
            middleware: None,
            schema: None,
            source_transform: None,
            installed_paths: Vec::new(),
            package_file_contents: std::collections::BTreeMap::new(),
            import_inputs: None,
        }
    }

    /// Snapshot traversal can reach the same Windows source through an ordinary
    /// path or a verbatim long path. That spelling alone is not a filesystem edit.
    pub(crate) fn eq_snapshot(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        #[cfg(windows)]
        {
            use crate::syncback::relocation_transaction::same_canonical_spelling;
            // Ordinary source/configuration edits need no comparison clone.
            if self.relevant_paths == other.relevant_paths
                && self.instigating_source == other.instigating_source
                && self.import_inputs == other.import_inputs
            {
                return false;
            }
            if !self.eq_import_inputs(other)
                || self.relevant_paths.len() != other.relevant_paths.len()
                || !self
                    .relevant_paths
                    .iter()
                    .zip(&other.relevant_paths)
                    .all(|(a, b)| same_canonical_spelling(a, b))
            {
                return false;
            }
            let mut equivalent = other.clone();
            equivalent.import_inputs.clone_from(&self.import_inputs);
            equivalent.relevant_paths.clone_from(&self.relevant_paths);
            if let (Some(InstigatingSource::Path(a)), Some(InstigatingSource::Path(b))) =
                (&self.instigating_source, &mut equivalent.instigating_source)
            {
                if same_canonical_spelling(a, b) {
                    b.clone_from(a);
                }
            }
            self == &equivalent
        }
        #[cfg(not(windows))]
        false
    }

    fn eq_import_inputs(&self, other: &Self) -> bool {
        if self.import_inputs == other.import_inputs {
            return true;
        }
        #[cfg(windows)]
        if let (Some(left), Some(right)) = (&self.import_inputs, &other.import_inputs) {
            use crate::syncback::relocation_transaction::same_canonical_spelling;

            if left.len() != right.len() {
                return false;
            }
            // Mixed namespace spellings can reorder the map. Match every probe once,
            // retaining exact bytes, absence, and even duplicate namespace evidence.
            let mut unmatched: Vec<_> = right.iter().collect();
            for (path, bytes) in left.iter() {
                let Some(index) = unmatched.iter().position(|(other_path, other_bytes)| {
                    bytes == *other_bytes && same_canonical_spelling(path, other_path)
                }) else {
                    return false;
                };
                unmatched.swap_remove(index);
            }
            return true;
        }
        false
    }

    pub(crate) fn has_same_structure(&self, other: &Self) -> bool {
        // Source transforms and expected bytes are refreshed after ordinary saves.
        // They do not change the instance's identity, ownership, or storage layout.
        self.ignore_unknown_instances == other.ignore_unknown_instances
            && self.instigating_source == other.instigating_source
            && self.relevant_paths == other.relevant_paths
            && self.context.has_same_structure(&other.context)
            && self.specified_id == other.specified_id
            && self.middleware == other.middleware
            && self.schema == other.schema
            && self.installed_paths == other.installed_paths
            && self.eq_import_inputs(other)
    }

    /// Served revisions are server-only proof, not a Source identity change or
    /// a client metadata edit. Still install their refreshed immutable witnesses.
    pub(crate) fn eq_without_served_projects(&self, other: &Self) -> bool {
        let mut context = self.context.clone();
        context.served_projects = other.context.served_projects.clone();
        self.has_same_structure(other)
            && self.source_transform == other.source_transform
            && self.package_file_contents == other.package_file_contents
            && context == other.context
    }

    pub(crate) fn watch_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.relevant_paths.iter().chain(&self.installed_paths)
    }

    pub(crate) fn is_package_mount_root(&self) -> bool {
        self.context
            .package_mount
            .as_ref()
            .is_some_and(|(source, _)| {
                self.relevant_paths.iter().any(|path| {
                    path == source
                        || (path.parent() == Some(source.as_path())
                            && crate::Project::is_project_file(path))
                })
            })
    }

    pub fn ignore_unknown_instances(self, ignore_unknown_instances: bool) -> Self {
        Self {
            ignore_unknown_instances,
            ..self
        }
    }

    pub fn instigating_source(self, instigating_source: impl Into<InstigatingSource>) -> Self {
        Self {
            instigating_source: Some(instigating_source.into()),
            ..self
        }
    }

    pub fn relevant_paths(self, relevant_paths: Vec<PathBuf>) -> Self {
        Self {
            relevant_paths,
            ..self
        }
    }

    pub fn context(self, context: &InstanceContext) -> Self {
        let mut context = context.clone();
        // Active traversal ancestry must not survive into a later watch reload.
        context.active_projects = None;
        context.active_directories = None;
        context.import_input_scope = Default::default();
        Self { context, ..self }
    }

    pub fn specified_id(self, id: Option<RojoRef>) -> Self {
        Self {
            specified_id: id,
            ..self
        }
    }

    pub fn middleware(self, middleware: Middleware) -> Self {
        Self {
            middleware: Some(middleware),
            ..self
        }
    }

    pub fn schema(self, schema: Option<String>) -> Self {
        Self { schema, ..self }
    }
}

impl Default for InstanceMetadata {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceTransform {
    pub original: String,
    pub emitted: String,
}

/// Holds context for `.luaurc` alias resolution. When present on an
/// `InstanceContext`, alias-based requires will be transformed into
/// runtime require-by-string paths, anchored for DataModel projects.
// `Eq` lets `Arc<RequireContext>` comparisons return early on pointer equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequireContext {
    /// The absolute path of the project root directory (where the
    /// .project.json file lives).
    #[serde(serialize_with = "path_serializer::serialize_absolute")]
    pub project_root: PathBuf,

    /// Maps normalized absolute filesystem paths to their DataModel path segments,
    /// including mounts outside the project root. Used to compute require paths that
    /// match the DataModel tree structure rather than the filesystem layout.
    ///
    /// For example, a project tree like:
    /// ```json
    /// { "Plugin": { "$path": "Source/Plugin" } }
    /// ```
    /// produces: `("<project-root>/Source/Plugin", ["Plugin"])`
    ///
    /// Private so replacements also reset the derived prefix cache.
    path_mappings: Vec<(PathBuf, Vec<String>)>,
    pub datamodel: bool,
    /// Lookup of `path_mappings` by path. Mappings cover every watched project
    /// path, so a linear scan per import made snapshotting quadratic.
    #[serde(skip)]
    mapping_prefixes: MappingPrefixCache,
}

impl RequireContext {
    pub fn new(
        project_root: PathBuf,
        path_mappings: Vec<(PathBuf, Vec<String>)>,
        datamodel: bool,
    ) -> Self {
        Self {
            project_root,
            path_mappings,
            datamodel,
            mapping_prefixes: MappingPrefixCache::default(),
        }
    }

    pub fn path_mappings(&self) -> &[(PathBuf, Vec<String>)] {
        &self.path_mappings
    }

    pub fn replace_path_mappings(&mut self, path_mappings: Vec<(PathBuf, Vec<String>)>) {
        self.path_mappings = path_mappings;
        self.mapping_prefixes = MappingPrefixCache::default();
    }

    pub(crate) fn mapping_prefixes(&self) -> MappingPrefixes {
        let mut cached = self.mapping_prefixes.0.lock().unwrap();
        cached
            .get_or_insert_with(|| {
                Arc::new(build_mapping_prefixes(
                    &self.path_mappings,
                    &self.project_root,
                ))
            })
            .clone()
    }
}

/// Maps a mapping path, or its extensionless script alias, to its first ordinal.
pub(crate) type MappingPrefixes = Arc<std::collections::HashMap<PathBuf, usize>>;

/// Derived state belongs to one controlled mapping list. Replacing that list or
/// cloning its context starts an empty cache, regardless of allocation or length.
#[derive(Default)]
struct MappingPrefixCache(std::sync::Mutex<Option<MappingPrefixes>>);

/// Normalizes every mapping against `root`, so relative and absolute mounts, including
/// those outside the project directory, are found by the same normalized lookup.
pub(crate) fn build_mapping_prefixes(
    mappings: &[(PathBuf, Vec<String>)],
    root: &Path,
) -> std::collections::HashMap<PathBuf, usize> {
    let mut prefixes = std::collections::HashMap::with_capacity(mappings.len());
    for (ordinal, (path, _)) in mappings.iter().enumerate() {
        let prefix = crate::package_sources::normalize(&root.join(path));
        if prefix
            .extension()
            .is_some_and(|extension| extension == "lua" || extension == "luau")
        {
            prefixes.entry(prefix.with_extension("")).or_insert(ordinal);
        }
        prefixes.entry(prefix).or_insert(ordinal);
    }
    prefixes
}

impl Clone for MappingPrefixCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for MappingPrefixCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for MappingPrefixCache {}

impl fmt::Debug for MappingPrefixCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MappingPrefixCache")
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ServedProjects(pub Arc<Vec<Arc<crate::Project>>>);

impl PartialEq for ServedProjects {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.len() == other.0.len()
                && self
                    .0
                    .iter()
                    .zip(other.0.iter())
                    .all(|(left, right)| Arc::ptr_eq(left, right) || left == right))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_ignore_rules: Arc<Vec<PathIgnoreRule>>,
    pub emit_legacy_scripts: bool,
    // Server-owned cache state cannot be re-included by project ignore rules.
    #[serde(skip)]
    reserved_cache_path: Option<PathBuf>,
    // Every instance's metadata keeps a context, and cloning a rule's compiled
    // glob regex allocates a fresh match cache pool, so the list is shared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync_rules: Arc<Vec<SyncRule>>,

    /// When set, `.luaurc` alias requires will be resolved during
    /// snapshotting for both build and serve sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_context: Option<Arc<RequireContext>>,

    // Branch-local project ancestry prevents speculative parallel work from recursing
    // forever through a project cycle while an earlier sibling reports an error.
    #[serde(skip)]
    pub(crate) active_projects: Option<Arc<Vec<PathBuf>>>,
    #[serde(skip)]
    pub(crate) active_directories: Option<Arc<Vec<PathBuf>>>,

    /// Immutable declarations that actually served this instance, not traversal state.
    /// Sharing both the ancestry and declarations keeps descendant contexts inexpensive.
    #[serde(skip)]
    pub(crate) served_projects: ServedProjects,

    /// Set only while reading one instance's own file for a two-way sync write.
    /// Directories then skip children stored in their own files, so checking an
    /// edit doesn't re-read a folder's entire subtree. Always cleared before the
    /// snapshot is compared or applied.
    #[serde(skip)]
    pub(crate) shallow: bool,

    /// Explicit filesystem-only sessions cannot accept writes and omit their proof.
    #[serde(skip)]
    pub(crate) omit_import_witness: bool,

    #[serde(skip)]
    pub(crate) import_input_scope: crate::snapshot_middleware::ImportInputScope,

    #[serde(skip)]
    pub(crate) package_sources: Option<Arc<crate::package_sources::PackageSources>>,
    #[serde(skip)]
    pub(crate) package_mount: Option<(PathBuf, PathBuf)>,
}

impl InstanceContext {
    pub fn new() -> Self {
        Self {
            path_ignore_rules: Arc::new(Vec::new()),
            reserved_cache_path: None,
            emit_legacy_scripts: emit_legacy_scripts_default().unwrap(),
            sync_rules: Arc::new(Vec::new()),
            require_context: None,
            active_projects: None,
            active_directories: None,
            served_projects: ServedProjects::default(),
            shallow: false,
            omit_import_witness: false,
            import_input_scope: Default::default(),
            package_sources: None,
            package_mount: None,
        }
    }

    fn has_same_structure(&self, other: &Self) -> bool {
        let Self {
            path_ignore_rules,
            emit_legacy_scripts,
            reserved_cache_path,
            sync_rules,
            require_context,
            active_projects,
            active_directories,
            served_projects: _,
            shallow,
            omit_import_witness,
            import_input_scope: _,
            package_sources,
            package_mount,
        } = self;
        // Global mappings drive import reemission, not this instance's physical
        // identity. Compare their namespace without revisiting every project path.
        path_ignore_rules == &other.path_ignore_rules
            && emit_legacy_scripts == &other.emit_legacy_scripts
            && reserved_cache_path == &other.reserved_cache_path
            && sync_rules == &other.sync_rules
            && match (require_context, &other.require_context) {
                (Some(left), Some(right)) => {
                    left.project_root == right.project_root && left.datamodel == right.datamodel
                }
                (None, None) => true,
                _ => false,
            }
            && active_projects == &other.active_projects
            && active_directories == &other.active_directories
            && shallow == &other.shallow
            && omit_import_witness == &other.omit_import_witness
            && package_sources == &other.package_sources
            && package_mount == &other.package_mount
    }

    pub fn with_emit_legacy_scripts(emit_legacy_scripts: Option<bool>) -> Self {
        Self {
            emit_legacy_scripts: emit_legacy_scripts
                .or_else(emit_legacy_scripts_default)
                .unwrap(),
            ..Self::new()
        }
    }

    pub fn set_reserved_cache_path(&mut self, path: PathBuf) {
        self.reserved_cache_path = Some(path);
    }

    pub fn reserved_cache_path(&self) -> Option<&Path> {
        self.reserved_cache_path.as_deref()
    }

    pub(crate) fn captures_import_inputs(&self) -> bool {
        self.reserved_cache_path.is_some() && !self.omit_import_witness
    }

    /// Returns whether the path lexically names the reserved cache or a descendant.
    /// Resolve ambiguous paths before using this to exclude snapshot sources.
    pub fn is_reserved_path(&self, path: &Path) -> bool {
        self.reserved_cache_path
            .as_ref()
            .is_some_and(|cache| path.starts_with(cache))
    }

    /// Extend the list of ignore rules in the context with the given new rules.
    pub fn add_path_ignore_rules<I>(&mut self, new_rules: I)
    where
        I: IntoIterator<Item = PathIgnoreRule>,
        I::IntoIter: ExactSizeIterator,
    {
        let new_rules = new_rules.into_iter();

        // If the iterator is empty, we can skip cloning our list of ignore
        // rules and appending to it.
        if new_rules.len() == 0 {
            return;
        }

        let rules = Arc::make_mut(&mut self.path_ignore_rules);
        rules.extend(new_rules);
    }

    /// Extend the list of syncing rules in the context with the given new rules.
    pub fn add_sync_rules<I>(&mut self, new_rules: I)
    where
        I: IntoIterator<Item = SyncRule>,
    {
        let mut new_rules = new_rules.into_iter().peekable();
        if new_rules.peek().is_none() {
            return;
        }

        Arc::make_mut(&mut self.sync_rules).extend(new_rules);
    }

    /// Clears all sync rules for this InstanceContext
    pub fn clear_sync_rules(&mut self) {
        self.sync_rules = Arc::new(Vec::new());
    }

    pub fn set_emit_legacy_scripts(&mut self, emit_legacy_scripts: bool) {
        self.emit_legacy_scripts = emit_legacy_scripts;
    }

    /// Returns the middleware specified by the first sync rule that
    /// matches the provided path. This does not handle default syncing rules.
    pub fn get_user_sync_rule(&self, path: &Path) -> Option<&SyncRule> {
        self.sync_rules.iter().find(|&rule| rule.matches(path))
    }

    /// Whether the effective interpretation is identical to the built-in mapping.
    /// File rules never select directory middleware. For files, both middleware and
    /// the pre-metadata name must agree, since that name also selects adjacent sidecars.
    pub(crate) fn has_default_sync_mapping(
        &self,
        path: &Path,
        is_directory: bool,
    ) -> anyhow::Result<bool> {
        if is_directory {
            return Ok(true);
        }
        let Some(rule) = self.get_user_sync_rule(path) else {
            return Ok(true);
        };
        let Some(default) = crate::snapshot_middleware::default_sync_rules()
            .iter()
            .find(|rule| rule.matches(path))
        else {
            return Ok(false);
        };
        Ok(rule.middleware == default.middleware
            && rule.file_name_for_path(path)? == default.file_name_for_path(path)?)
    }
}

impl Default for InstanceContext {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathIgnoreRule {
    /// The path that this glob is relative to. Since ignore globs are defined
    /// in project files, this will generally be the folder containing the
    /// project file that defined this glob.
    #[serde(serialize_with = "path_serializer::serialize_absolute")]
    pub base_path: PathBuf,

    /// The actual glob that can be matched against the input path.
    pub glob: IgnorableGlob,
}

impl PathIgnoreRule {
    pub fn matches<P: AsRef<Path>>(&self, path: P) -> bool {
        let path = path.as_ref();

        match path.strip_prefix(&self.base_path) {
            Ok(suffix) => self.glob.is_match(suffix),
            Err(_) => false,
        }
    }

    pub fn is_negation(&self) -> bool {
        self.glob.is_negation()
    }
}

/// Evaluates an ordered list of [`PathIgnoreRule`]s against a path using
/// gitignore-style "last match wins" semantics: a path is ignored if the last
/// rule whose pattern matches it is non-negated. Paths matched by no rule are
/// not ignored.
pub fn is_path_ignored<P: AsRef<Path>>(rules: &[PathIgnoreRule], path: P) -> bool {
    let path = path.as_ref();
    let mut ignored = false;
    for rule in rules {
        if rule.matches(path) {
            ignored = !rule.is_negation();
        }
    }
    ignored
}

/// Represents where a particular Instance or InstanceSnapshot came from.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum InstigatingSource {
    /// The path the Instance was made from.
    Path(#[serde(serialize_with = "path_serializer::serialize_absolute")] PathBuf),
    /// The node in a Project that the Instance was made from.
    ProjectNode {
        #[serde(serialize_with = "path_serializer::serialize_absolute")]
        path: PathBuf,
        name: String,
        node: ProjectNode,
        parent_class: Option<String>,
    },
}

impl InstigatingSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) => path.as_path(),
            Self::ProjectNode { path, .. } => path.as_path(),
        }
    }
}

impl fmt::Debug for InstigatingSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstigatingSource::Path(path) => write!(formatter, "Path({})", path.display()),
            InstigatingSource::ProjectNode {
                name,
                node,
                path,
                parent_class,
            } => write!(
                formatter,
                "ProjectNode({}: {:?}) from path {} and parent class {:?}",
                name,
                node,
                path.display(),
                parent_class,
            ),
        }
    }
}

impl From<PathBuf> for InstigatingSource {
    fn from(path: PathBuf) -> Self {
        InstigatingSource::Path(path)
    }
}

impl From<&Path> for InstigatingSource {
    fn from(path: &Path) -> Self {
        InstigatingSource::Path(path.to_path_buf())
    }
}

/// Represents an user-specified rule for transforming files
/// into Instances using a given middleware.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct SyncRule {
    /// A pattern used to determine if a file is included in this SyncRule
    #[serde(rename = "pattern")]
    pub include: Glob,
    /// A pattern used to determine if a file is excluded from this SyncRule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Glob>,
    /// The middleware specified by the user for this SyncRule
    #[serde(rename = "use", deserialize_with = "SyncRule::deserialize_middleware")]
    pub middleware: Middleware,
    /// A suffix to trim off of file names, including the file extension.
    /// If not specified, the file extension is the only thing cut off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    /// The 'base' of the glob above, allowing it to be used
    /// relative to a path instead of absolute.
    #[serde(skip)]
    pub base_path: PathBuf,
}

impl SyncRule {
    fn deserialize_middleware<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Middleware, D::Error> {
        let middleware = Middleware::deserialize(deserializer)?;
        if middleware.is_dir() {
            return Err(serde::de::Error::custom(
                "Directory middleware cannot be selected by a file sync rule",
            ));
        }
        Ok(middleware)
    }

    /// Returns whether the given path matches this rule.
    pub fn matches(&self, path: &Path) -> bool {
        match path.strip_prefix(&self.base_path) {
            Ok(suffix) => {
                if let Some(pattern) = &self.exclude {
                    if pattern.is_match(suffix) {
                        return false;
                    }
                }
                self.include.is_match(suffix)
            }
            Err(_) => false,
        }
    }

    pub fn file_name_for_path<'a>(&self, path: &'a Path) -> anyhow::Result<&'a str> {
        if let Some(suffix) = &self.suffix {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .with_context(|| format!("file name of {} is invalid", path.display()))?;
            if file_name.ends_with(suffix) {
                let end = file_name.len().saturating_sub(suffix.len());
                Ok(&file_name[..end])
            } else {
                Ok(file_name)
            }
        } else {
            // If the user doesn't specify a suffix, we assume they just want
            // the name of the file (the file_stem)
            path.file_stem()
                .and_then(|s| s.to_str())
                .with_context(|| format!("file name of {} is invalid", path.display()))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn compact_instance_context_round_trips_omitted_empty_rules() {
        let context = InstanceContext::default();
        let json = serde_json::to_vec(&context).unwrap();
        assert_eq!(
            serde_json::from_slice::<InstanceContext>(&json).unwrap(),
            context
        );
        let msgpack = rmp_serde::to_vec_named(&context).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<InstanceContext>(&msgpack).unwrap(),
            context
        );
    }

    #[test]
    fn directory_middleware_round_trips_in_metadata_but_not_file_rules() {
        for middleware in [
            Middleware::Dir,
            Middleware::ServerScriptDir,
            Middleware::ClientScriptDir,
            Middleware::LegacyServerScriptDir,
            Middleware::LegacyClientScriptDir,
            Middleware::PluginScriptDir,
            Middleware::ModuleScriptDir,
            Middleware::CsvDir,
            Middleware::ModuleScript,
        ] {
            let mut metadata = InstanceMetadata::new();
            metadata.middleware = Some(middleware);
            let json = serde_json::to_vec(&metadata).unwrap();
            assert_eq!(
                serde_json::from_slice::<InstanceMetadata>(&json).unwrap(),
                metadata
            );
            let msgpack = rmp_serde::to_vec_named(&metadata).unwrap();
            assert_eq!(
                rmp_serde::from_slice::<InstanceMetadata>(&msgpack).unwrap(),
                metadata
            );
            let rule = serde_json::json!({"pattern":"*", "use":middleware});
            assert_eq!(
                serde_json::from_value::<SyncRule>(rule).is_err(),
                middleware.is_dir()
            );
        }
    }

    #[test]
    fn import_witness_is_server_only_and_changes_interpretation_not_payload() {
        let original = InstanceMetadata::new();
        let mut changed = original.clone();
        changed.import_inputs = Some(Arc::new(
            [
                (
                    PathBuf::from("/project/.luaurc"),
                    Some(Arc::new(b"{}".to_vec())),
                ),
                (PathBuf::from("/project/Source/.luaurc"), None),
            ]
            .into_iter()
            .collect(),
        ));
        assert!(!original.has_same_structure(&changed));
        assert!(!changed.eq_without_served_projects(&original));
        let mut forged = serde_json::to_value(&original).unwrap();
        forged["import_inputs"] = serde_json::json!({"/project/.luaurc": [1, 2, 3]});
        for encoded in [serde_json::to_value(&changed).unwrap(), forged] {
            assert_eq!(
                serde_json::from_value::<InstanceMetadata>(encoded).unwrap(),
                original
            );
        }
        let encoded = rmp_serde::to_vec_named(&changed).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<InstanceMetadata>(&encoded).unwrap(),
            original
        );
        assert_eq!(changed.watch_paths().count(), 0);
    }

    #[test]
    fn import_witness_preserves_bytes_absence_paths_and_capture_state() {
        let mut original = InstanceMetadata::new();
        original.import_inputs = Some(Arc::new(
            [
                (
                    PathBuf::from("/project/.luaurc"),
                    Some(Arc::new(b"{}".to_vec())),
                ),
                (PathBuf::from("/project/Source/.luaurc"), None),
            ]
            .into_iter()
            .collect(),
        ));
        for change in 0..7 {
            let mut changed = original.clone();
            let inputs = Arc::make_mut(changed.import_inputs.as_mut().unwrap());
            match change {
                0 => {
                    *inputs.get_mut(Path::new("/project/.luaurc")).unwrap() =
                        Some(Arc::new(b"{\"aliases\":{}}".to_vec()))
                }
                1 => *inputs.get_mut(Path::new("/project/.luaurc")).unwrap() = None,
                2 => {
                    *inputs
                        .get_mut(Path::new("/project/Source/.luaurc"))
                        .unwrap() = Some(Arc::new(Vec::new()))
                }
                3 => {
                    inputs.remove(Path::new("/project/Source/.luaurc"));
                }
                4 => {
                    inputs.remove(Path::new("/project/Source/.luaurc"));
                    inputs.insert("/project/Other/.luaurc".into(), None);
                }
                5 => inputs.clear(),
                6 => changed.import_inputs = None,
                _ => unreachable!(),
            }
            for (left, right) in [(&original, &changed), (&changed, &original)] {
                assert!(!left.eq_snapshot(right), "change {change}");
                assert!(!left.has_same_structure(right), "change {change}");
                assert!(!left.eq_without_served_projects(right), "change {change}");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_import_witness_namespace_only_keys_are_equivalent() {
        for (ordinary, verbatim) in [
            (
                r"C:\project\Source\.luaurc",
                r"\\?\C:\project\Source\.luaurc",
            ),
            (
                r"\\server\share\project\.luaurc",
                r"\\?\UNC\server\share\project\.luaurc",
            ),
        ] {
            for bytes in [None, Some(Arc::new(b"{}".to_vec()))] {
                let mut original = InstanceMetadata::new();
                original.import_inputs = Some(Arc::new(
                    [(PathBuf::from(ordinary), bytes.clone())]
                        .into_iter()
                        .collect(),
                ));
                let mut changed = original.clone();
                changed.import_inputs = Some(Arc::new(
                    [(PathBuf::from(verbatim), bytes.clone())]
                        .into_iter()
                        .collect(),
                ));
                assert_ne!(original, changed);
                for (left, right) in [(&original, &changed), (&changed, &original)] {
                    assert!(left.eq_snapshot(right), "{ordinary}");
                    assert!(left.has_same_structure(right), "{ordinary}");
                    assert!(left.eq_without_served_projects(right), "{ordinary}");
                }
                for replacement in [None, Some(Arc::new(b"{\"aliases\":{}}".to_vec()))] {
                    if replacement == bytes {
                        continue;
                    }
                    let mut edited = changed.clone();
                    Arc::make_mut(edited.import_inputs.as_mut().unwrap())
                        .insert(PathBuf::from(verbatim), replacement);
                    for (left, right) in [(&original, &edited), (&edited, &original)] {
                        assert!(!left.eq_snapshot(right), "{ordinary}");
                        assert!(!left.has_same_structure(right), "{ordinary}");
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_import_witness_mixed_namespace_order_preserves_every_probe() {
        let mut original = InstanceMetadata::new();
        original.import_inputs = Some(Arc::new(
            [
                (
                    PathBuf::from(r"C:\project\A\.luaurc"),
                    Some(Arc::new(b"{}".to_vec())),
                ),
                (PathBuf::from(r"\\?\C:\project\B\.luaurc"), None),
            ]
            .into_iter()
            .collect(),
        ));
        let mut changed = original.clone();
        changed.import_inputs = Some(Arc::new(
            [
                (
                    PathBuf::from(r"\\?\C:\project\A\.luaurc"),
                    Some(Arc::new(b"{}".to_vec())),
                ),
                (PathBuf::from(r"C:\project\B\.luaurc"), None),
            ]
            .into_iter()
            .collect(),
        ));
        for (left, right) in [(&original, &changed), (&changed, &original)] {
            assert!(left.eq_snapshot(right));
            assert!(left.has_same_structure(right));
        }
        let inputs = Arc::make_mut(changed.import_inputs.as_mut().unwrap());
        inputs.remove(Path::new(r"C:\project\B\.luaurc"));
        inputs.insert(PathBuf::from(r"C:\project\A\.luaurc"), None);
        assert!(!original.eq_snapshot(&changed));
        assert!(!original.has_same_structure(&changed));
    }

    #[cfg(windows)]
    #[test]
    fn windows_import_witness_namespace_equivalence_keeps_strict_path_distinctions() {
        for (left, right) in [
            (
                r"C:\project\Source\.luaurc",
                r"\\?\C:\project\Other\.luaurc",
            ),
            (
                r"C:\project\Source\.luaurc",
                r"\\?\C:\project\source\.luaurc",
            ),
            (
                r"C:\project\Source\.luaurc",
                r"\\?\D:\project\Source\.luaurc",
            ),
            (r"C:\project\Source\.luaurc", r"\\?\C:\project\Link\.luaurc"),
            (
                r"C:\project\Source\.luaurc",
                r"\\?\C:\project\Other\..\Source\.luaurc",
            ),
            (r"C:\project\NUL\.luaurc", r"\\?\C:\project\NUL\.luaurc"),
            (
                r"C:\project\CON.txt\.luaurc",
                r"\\?\C:\project\CON.txt\.luaurc",
            ),
            (
                r"C:\project\Source.\.luaurc",
                r"\\?\C:\project\Source.\.luaurc",
            ),
            (
                r"C:\project\Source \.luaurc",
                r"\\?\C:\project\Source \.luaurc",
            ),
            (
                r"\\server\share\project\.luaurc",
                r"\\?\UNC\server\other\project\.luaurc",
            ),
        ] {
            let mut original = InstanceMetadata::new();
            original.import_inputs = Some(Arc::new(
                [(PathBuf::from(left), None)].into_iter().collect(),
            ));
            let mut changed = original.clone();
            changed.import_inputs = Some(Arc::new(
                [(PathBuf::from(right), None)].into_iter().collect(),
            ));
            for (left, right) in [(&original, &changed), (&changed, &original)] {
                assert!(!left.eq_snapshot(right));
                assert!(!left.has_same_structure(right));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_path_plain_module_import_witness_snapshot_namespace_equivalence() {
        use crate::snapshot_middleware::snapshot_from_vfs;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let verbatim = root
            .join("long-source-directory".repeat(12))
            .join("nested")
            .join("Module.lua");
        let ordinary = PathBuf::from(verbatim.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
        assert!(ordinary.as_os_str().len() > 260);
        assert_ne!(ordinary, verbatim);
        std::fs::create_dir_all(verbatim.parent().unwrap()).unwrap();
        std::fs::write(&verbatim, "return 1").unwrap();
        std::fs::write(root.join(".luaurc"), "{}").unwrap();
        let mut context = InstanceContext::new();
        context.set_reserved_cache_path(root.join(".rojo"));
        assert!(context.require_context.is_none());
        assert!(context.captures_import_inputs());
        let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let original = snapshot_from_vfs(&context, &vfs, &ordinary)
            .unwrap()
            .unwrap();
        let changed = snapshot_from_vfs(&context, &vfs, &verbatim)
            .unwrap()
            .unwrap();
        for (snapshot, path) in [(&original, &ordinary), (&changed, &verbatim)] {
            assert_eq!(snapshot.class_name, "ModuleScript");
            assert_eq!(
                snapshot.properties[&rbx_dom_weak::ustr("Source")],
                rbx_dom_weak::types::Variant::String("return 1".into())
            );
            assert!(snapshot.metadata.context.require_context.is_none());
            let inputs = snapshot.metadata.import_inputs.as_ref().unwrap();
            assert_eq!(
                inputs.get(&path.parent().unwrap().join(".luaurc")),
                Some(&None)
            );
            assert!(inputs
                .values()
                .any(|bytes| bytes.as_deref().is_some_and(|bytes| bytes == b"{}")));
        }
        assert_ne!(
            original.metadata.import_inputs,
            changed.metadata.import_inputs
        );
        for (before, after) in [(&original, &changed), (&changed, &original)] {
            let tree = crate::snapshot::RojoTree::new(before.clone());
            let patch =
                crate::snapshot::compute_patch_set(Some(after.clone()), &tree, tree.get_root_id());
            assert_eq!(patch, crate::snapshot::PatchSet::new());
        }
        for (left, right) in [
            (&original.metadata, &changed.metadata),
            (&changed.metadata, &original.metadata),
        ] {
            assert!(left.eq_snapshot(right));
            // Hold the source spelling fixed to isolate witness-only structural changes.
            let mut same_source = right.clone();
            same_source
                .instigating_source
                .clone_from(&left.instigating_source);
            same_source.relevant_paths.clone_from(&left.relevant_paths);
            assert!(left.has_same_structure(&same_source));
            assert!(left.eq_without_served_projects(&same_source));
        }
    }

    #[test]
    fn plain_module_import_witness_config_edits_remain_structural() {
        use crate::snapshot_middleware::snapshot_from_vfs;

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let path = root.join("Module.lua");
        let config = root.join(".luaurc");
        std::fs::write(&path, "return 1").unwrap();
        std::fs::write(&config, "{}").unwrap();
        let mut context = InstanceContext::new();
        context.set_reserved_cache_path(root.join(".rojo"));
        let snapshot = || {
            let vfs = memofs::Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            snapshot_from_vfs(&context, &vfs, &path).unwrap().unwrap()
        };
        let original = snapshot();
        std::fs::write(&config, "{\"aliases\":{}}").unwrap();
        let bytes_changed = snapshot();
        std::fs::remove_file(&config).unwrap();
        let absent = snapshot();
        for changed in [&bytes_changed, &absent] {
            assert_eq!(original.properties, changed.properties);
            assert!(!original.metadata.eq_snapshot(&changed.metadata));
            assert!(!original.metadata.has_same_structure(&changed.metadata));
        }
        assert!(!bytes_changed.metadata.has_same_structure(&absent.metadata));
    }

    #[test]
    fn structural_metadata_ignores_derived_require_mapping_changes() {
        let mut original = InstanceMetadata::new();
        original.context.require_context = Some(Arc::new(RequireContext::new(
            "/project".into(),
            vec![("Source/Actor".into(), vec!["Actor".into()])],
            true,
        )));
        let mut equal = original.clone();
        equal.context.require_context = Some(Arc::new(
            original.context.require_context.as_deref().unwrap().clone(),
        ));
        assert!(!Arc::ptr_eq(
            original.context.require_context.as_ref().unwrap(),
            equal.context.require_context.as_ref().unwrap()
        ));
        assert_eq!(original, equal);
        assert!(original.has_same_structure(&equal));
        assert!(equal.has_same_structure(&original));

        let mut changed = equal;
        Arc::make_mut(changed.context.require_context.as_mut().unwrap()).replace_path_mappings(
            vec![(
                "Source/Actor".into(),
                vec!["Destination".into(), "Actor".into()],
            )],
        );
        assert_ne!(original.context, changed.context);
        assert_ne!(original, changed);
        for _ in 0..2 {
            assert!(original.has_same_structure(&changed));
            assert!(changed.has_same_structure(&original));
        }
    }

    #[test]
    fn structural_metadata_preserves_every_local_context_field() {
        let mut original = InstanceMetadata::new();
        original.context.require_context = Some(Arc::new(RequireContext::new(
            "/project".into(),
            vec![("Source/Actor".into(), vec!["Actor".into()])],
            true,
        )));
        let package_sources: crate::package_sources::PackageSources =
            serde_json::from_value(serde_json::json!({
                "root": "/project",
                "install": null,
                "problem": null,
                "local_sources": [],
                "deny_all_writes": false
            }))
            .unwrap();
        let package_sources = Arc::new(package_sources);
        for flags in 0..8192 {
            let mut changed = original.clone();
            let require_context = Arc::make_mut(changed.context.require_context.as_mut().unwrap());
            require_context.replace_path_mappings(Vec::new());
            if flags & 1 != 0 {
                changed.context.path_ignore_rules = Arc::new(vec![PathIgnoreRule {
                    base_path: "/project".into(),
                    glob: IgnorableGlob::new("Ignored/**").unwrap(),
                }]);
            }
            if flags & 2 != 0 {
                changed.context.emit_legacy_scripts = !changed.context.emit_legacy_scripts;
            }
            if flags & 4 != 0 {
                changed.context.reserved_cache_path = Some("/project/.rojo".into());
            }
            if flags & 8 != 0 {
                changed.context.sync_rules = Arc::new(vec![rule("*.text")]);
            }
            if flags & 32 != 0 {
                Arc::make_mut(changed.context.require_context.as_mut().unwrap()).project_root =
                    "/other-project".into();
            }
            if flags & 64 != 0 {
                Arc::make_mut(changed.context.require_context.as_mut().unwrap()).datamodel = false;
            }
            if flags & 16 != 0 {
                changed.context.require_context = None;
            }
            if flags & 128 != 0 {
                changed.context.active_projects = Some(Arc::new(vec!["/project".into()]));
            }
            if flags & 256 != 0 {
                changed.context.active_directories = Some(Arc::new(vec!["/project/Source".into()]));
            }
            if flags & 512 != 0 {
                changed.context.shallow = true;
            }
            if flags & 1024 != 0 {
                changed.context.package_sources = Some(Arc::clone(&package_sources));
            }
            if flags & 2048 != 0 {
                changed.context.package_mount =
                    Some(("/project/Packages/Local".into(), "/project/Source".into()));
            }
            if flags & 4096 != 0 {
                changed.context.omit_import_witness = true;
            }
            for _ in 0..2 {
                assert_eq!(
                    original.has_same_structure(&changed),
                    flags == 0,
                    "flags {flags}"
                );
                assert_eq!(
                    changed.has_same_structure(&original),
                    flags == 0,
                    "flags {flags}"
                );
            }
        }
    }

    #[test]
    fn structural_metadata_package_context_compares_policy_values() {
        let package_sources = serde_json::json!({
            "root": "/project",
            "install": null,
            "problem": null,
            "local_sources": [],
            "deny_all_writes": false
        });
        let mut original = InstanceMetadata::new();
        original.context.package_sources = Some(Arc::new(
            serde_json::from_value(package_sources.clone()).unwrap(),
        ));
        let mut equal = original.clone();
        equal.context.package_sources = Some(Arc::new(
            serde_json::from_value(package_sources.clone()).unwrap(),
        ));
        assert!(!Arc::ptr_eq(
            original.context.package_sources.as_ref().unwrap(),
            equal.context.package_sources.as_ref().unwrap()
        ));
        assert_eq!(original, equal);
        assert!(original.has_same_structure(&equal));
        for (field, value) in [
            ("root", serde_json::json!("/other-project")),
            ("problem", serde_json::json!("Invalid generation")),
            ("local_sources", serde_json::json!(["/project/Source"])),
            ("deny_all_writes", serde_json::json!(true)),
            (
                "install",
                serde_json::json!({
                    "version": 1,
                    "manifest_hash": "generation",
                    "local_manifests": {},
                    "package_roots": [],
                    "generated_paths": []
                }),
            ),
        ] {
            let mut changed_sources = package_sources.clone();
            changed_sources[field] = value;
            let mut changed = original.clone();
            changed.context.package_sources =
                Some(Arc::new(serde_json::from_value(changed_sources).unwrap()));
            assert_ne!(original, changed);
            assert!(!original.has_same_structure(&changed), "{field}");
            assert!(!changed.has_same_structure(&original), "{field}");
        }
    }

    #[test]
    fn structural_metadata_classification_ignores_only_save_bookkeeping() {
        let original = InstanceMetadata::new();
        for flags in 0..256 {
            let mut changed = original.clone();
            if flags & 1 != 0 {
                changed.ignore_unknown_instances = true;
            }
            if flags & 2 != 0 {
                changed.instigating_source =
                    Some(InstigatingSource::Path("/project/Source".into()));
            }
            if flags & 4 != 0 {
                changed.relevant_paths.push("/project/Source".into());
            }
            if flags & 8 != 0 {
                changed.context.emit_legacy_scripts = !changed.context.emit_legacy_scripts;
            }
            if flags & 16 != 0 {
                changed.specified_id = Some(RojoRef::new("identity".into()));
            }
            if flags & 32 != 0 {
                changed.middleware = Some(Middleware::Text);
            }
            if flags & 64 != 0 {
                changed.schema = Some("schema".into());
            }
            if flags & 128 != 0 {
                changed.installed_paths.push("/project/Installed".into());
            }
            changed.source_transform = Some(SourceTransform {
                original: "return require(\"@target\")".into(),
                emitted: "return require(\"./Target\")".into(),
            });
            changed
                .package_file_contents
                .insert("/project/Source".into(), Some(vec![1, 2, 3]));
            for _ in 0..2 {
                assert_eq!(
                    original.has_same_structure(&changed),
                    flags == 0,
                    "flags {flags}"
                );
                assert_eq!(
                    changed.has_same_structure(&original),
                    flags == 0,
                    "flags {flags}"
                );
            }
        }
    }

    fn rule(pattern: &str) -> SyncRule {
        SyncRule {
            include: Glob::new(pattern).unwrap(),
            exclude: None,
            middleware: Middleware::Text,
            suffix: None,
            base_path: PathBuf::new(),
        }
    }

    #[test]
    fn relocation_mapping_equivalence_compares_effective_names_and_middleware() {
        for (filename, middleware, suffix) in [
            ("Code.luau", Middleware::ModuleScript, ".luau"),
            ("Code.lua", Middleware::ModuleScript, ".lua"),
            (
                "Code.server.luau",
                Middleware::RunContextServerScript,
                ".server.luau",
            ),
            (
                "Code.client.lua",
                Middleware::RunContextClientScript,
                ".client.lua",
            ),
            (
                "Code.local.luau",
                Middleware::LegacyClientScript,
                ".local.luau",
            ),
            (
                "Code.legacy.lua",
                Middleware::LegacyServerScript,
                ".legacy.lua",
            ),
            ("Code.plugin.luau", Middleware::PluginScript, ".plugin.luau"),
            ("Code.txt", Middleware::Text, ".txt"),
            ("Code.jsonc", Middleware::Json, ".jsonc"),
            ("Code.model.json", Middleware::JsonModel, ".model.json"),
            ("Code.csv", Middleware::Csv, ".csv"),
            ("Code.toml", Middleware::Toml, ".toml"),
            ("Code.yml", Middleware::Yaml, ".yml"),
        ] {
            let path = PathBuf::from("/project/src").join(filename);
            for wrong_middleware in [false, true] {
                for trim in [None, Some(suffix), Some(".not-a-suffix"), Some(filename)] {
                    let mut context = InstanceContext::new();
                    context.add_sync_rules([SyncRule {
                        include: Glob::new("*").unwrap(),
                        exclude: None,
                        middleware: if wrong_middleware {
                            Middleware::Ignore
                        } else {
                            middleware
                        },
                        suffix: trim.map(str::to_owned),
                        base_path: "/project".into(),
                    }]);
                    let expected = !wrong_middleware
                        && (trim == Some(suffix)
                            || (trim.is_none() && path.file_stem().unwrap() == "Code"));
                    for _ in 0..2 {
                        assert_eq!(
                            context.has_default_sync_mapping(&path, false).unwrap(),
                            expected,
                            "{filename} {trim:?} {wrong_middleware}"
                        );
                        // File rules do not participate in directory interpretation,
                        // even when a directory's name looks like a script or sidecar.
                        assert!(context.has_default_sync_mapping(&path, true).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn relocation_mapping_equivalence_respects_precedence_exclusions_and_sidecars() {
        let mut context = InstanceContext::new();
        let path = Path::new("/project/src/Code.local.luau");
        let equivalent = SyncRule {
            include: Glob::new("*.local.luau").unwrap(),
            exclude: None,
            middleware: Middleware::LegacyClientScript,
            suffix: Some(".local.luau".into()),
            base_path: "/project".into(),
        };
        let mut custom = equivalent.clone();
        custom.suffix = None;
        context.add_sync_rules([equivalent.clone(), custom.clone()]);
        assert!(context.has_default_sync_mapping(path, false).unwrap());
        context.clear_sync_rules();
        context.add_sync_rules([custom.clone(), equivalent.clone()]);
        assert!(!context.has_default_sync_mapping(path, false).unwrap());
        custom.exclude = Some(Glob::new("src/**").unwrap());
        context.clear_sync_rules();
        context.add_sync_rules([custom, equivalent]);
        assert!(context.has_default_sync_mapping(path, false).unwrap());
        assert!(context
            .has_default_sync_mapping(Path::new("/elsewhere/Code.local.luau"), false)
            .unwrap());
        context.add_sync_rules([SyncRule {
            include: Glob::new("*").unwrap(),
            exclude: None,
            middleware: Middleware::Text,
            suffix: None,
            base_path: "/project".into(),
        }]);
        for filename in ["Code.meta.json", "Code.meta.jsonc", "unmanaged.bin"] {
            assert!(
                !context
                    .has_default_sync_mapping(&PathBuf::from("/project/src").join(filename), false)
                    .unwrap(),
                "{filename}"
            );
        }
        context.clear_sync_rules();
        for filename in ["Code.meta.json", "Code.meta.jsonc", "unmanaged.bin"] {
            assert!(context
                .has_default_sync_mapping(&PathBuf::from("/project/src").join(filename), false)
                .unwrap());
        }
    }

    #[test]
    fn reserved_cache_matches_only_its_directory_and_descendants() {
        let mut context = InstanceContext::new();
        assert!(context.reserved_cache_path().is_none());
        assert!(!context.is_reserved_path(Path::new("/project/.rojo")));
        context.set_reserved_cache_path(PathBuf::from("/project/.rojo"));
        assert_eq!(
            context.reserved_cache_path(),
            Some(Path::new("/project/.rojo"))
        );

        for path in ["/project/.rojo", "/project/.rojo/relocations/intent.json"] {
            assert!(context.is_reserved_path(Path::new(path)), "{path}");
        }
        for path in [
            "/project",
            "/project/.rojo-backup/intent.json",
            "/project/Source/.rojo/intent.json",
            "/other/.rojo/intent.json",
        ] {
            assert!(!context.is_reserved_path(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn reserved_cache_survives_cloning_and_metadata_context_retention() {
        let mut context = InstanceContext::new();
        context.set_reserved_cache_path(PathBuf::from("/project/.rojo"));
        context.active_projects = Some(Arc::new(vec![PathBuf::from("/project")]));
        context.active_directories = Some(Arc::new(vec![PathBuf::from("/project")]));
        let cloned = context.clone();
        let metadata = InstanceMetadata::new().context(&context);
        assert!(metadata.context.active_projects.is_none());
        assert!(metadata.context.active_directories.is_none());
        for retained in [&cloned, &metadata.context] {
            assert!(retained.is_reserved_path(Path::new("/project/.rojo/intent.json")));
        }
    }

    #[test]
    fn ordinary_ignore_negation_still_uses_the_last_matching_rule() {
        let rules = ["*", "!*", "hidden.txt"].map(|pattern| PathIgnoreRule {
            base_path: PathBuf::from("/project"),
            glob: IgnorableGlob::new(pattern).unwrap(),
        });
        assert!(!is_path_ignored(&rules, "/project/visible.txt"));
        assert!(is_path_ignored(&rules, "/project/hidden.txt"));
    }

    #[test]
    fn cloned_contexts_share_sync_rules_until_changed() {
        let mut parent = InstanceContext::new();
        parent.add_sync_rules([rule("*.text")]);

        let mut child = parent.clone();
        assert!(Arc::ptr_eq(&parent.sync_rules, &child.sync_rules));

        child.add_sync_rules(std::iter::empty());
        assert!(Arc::ptr_eq(&parent.sync_rules, &child.sync_rules));

        child.add_sync_rules([rule("*.data")]);
        assert!(!Arc::ptr_eq(&parent.sync_rules, &child.sync_rules));
        assert_eq!(parent.sync_rules.len(), 1);
        assert_eq!(child.sync_rules.len(), 2);

        child.clear_sync_rules();
        assert!(child.sync_rules.is_empty());
        assert_eq!(parent.sync_rules.len(), 1);
        assert!(parent.get_user_sync_rule(Path::new("a.text")).is_some());
    }
}
