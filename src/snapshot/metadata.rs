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
}

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
        }
    }

    pub(crate) fn has_same_structure(&self, other: &Self) -> bool {
        // Source transforms and expected bytes are refreshed after ordinary saves.
        // They do not change the instance's identity, ownership, or storage layout.
        self.ignore_unknown_instances == other.ignore_unknown_instances
            && self.instigating_source == other.instigating_source
            && self.relevant_paths == other.relevant_paths
            && self.context == other.context
            && self.specified_id == other.specified_id
            && self.middleware == other.middleware
            && self.schema == other.schema
            && self.installed_paths == other.installed_paths
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequireContext {
    /// The absolute path of the project root directory (where the
    /// .project.json file lives).
    #[serde(serialize_with = "path_serializer::serialize_absolute")]
    pub project_root: PathBuf,

    /// Maps filesystem paths (relative to project root) to their
    /// DataModel path segments. Used to compute relative require
    /// paths that match the DataModel tree structure rather than
    /// the filesystem layout.
    ///
    /// For example, a project tree like:
    /// ```json
    /// { "Plugin": { "$path": "Source/Plugin" } }
    /// ```
    /// produces: `("Source/Plugin", ["Plugin"])`
    pub path_mappings: Vec<(PathBuf, Vec<String>)>,
    pub datamodel: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceContext {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path_ignore_rules: Arc<Vec<PathIgnoreRule>>,
    pub emit_legacy_scripts: bool,
    // Server-owned cache state cannot be re-included by project ignore rules.
    #[serde(skip)]
    reserved_cache_path: Option<PathBuf>,
    // Every instance's metadata keeps a context, and cloning a rule's compiled
    // glob regex allocates a fresh match cache pool, so the list is shared.
    #[serde(skip_serializing_if = "Vec::is_empty")]
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

    /// Set only while reading one instance's own file for a two-way sync write.
    /// Directories then skip children stored in their own files, so checking an
    /// edit doesn't re-read a folder's entire subtree. Always cleared before the
    /// snapshot is compared or applied.
    #[serde(skip)]
    pub(crate) shallow: bool,

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
            shallow: false,
            package_sources: None,
            package_mount: None,
        }
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
    #[serde(rename = "use")]
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
