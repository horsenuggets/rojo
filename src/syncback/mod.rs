mod addition;
mod file_names;
pub mod from_studio;
mod fs_snapshot;
mod hash;
mod inline;
pub(crate) mod lfs;
mod model_trees;
pub(crate) mod promotion;
mod property_filter;
mod ref_properties;
pub(crate) mod relocation;
pub mod relocation_transaction;
mod snapshot;
mod write_decision;

use anyhow::Context;
use indexmap::IndexMap;
use memofs::{IoResultExt, Vfs};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Instance, Ustr, UstrSet, WeakDom,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    glob::{Glob, IgnorableGlob},
    project::DEFAULT_PROJECT_NAMES,
    resolution::canonical_property_name,
    snapshot::{InstanceWithMeta, RojoTree},
    snapshot_middleware::{dir_init_files, Middleware},
    syncback::ref_properties::{attribute_ids, collect_referents, ids_on_disk, link_referents},
    Project, REF_POINTER_ATTRIBUTE_PREFIX,
};

pub use addition::{drop_duplicate_ids, drop_ignored_properties, syncback_new_subtree};
pub use file_names::{
    directory_middleware_for_instance, extension_for_middleware, name_for_inst, sibling_order_key,
    unique_child_stems, validate_file_name,
};
pub use fs_snapshot::FsSnapshot;
pub use hash::*;
pub(crate) use inline::is_script_class;
pub use model_trees::{is_model_file, is_project_owned, ModelTreeRule, ModelTrees};
pub(crate) use model_trees::{stale_layouts, CompiledModelTreeRules};
pub(crate) use property_filter::{can_write_property, is_unset_value};
pub use property_filter::{
    filter_json_properties_preallocated, filter_properties, filter_properties_preallocated,
};
pub use snapshot::{SyncbackData, SyncbackSnapshot};
pub use write_decision::{decide_property_write, same_value, PropertyWriteDecision};

/// The name of an enviroment variable to use to override the behavior of
/// syncback on model files.
/// By default, syncback will use `Rbxm` for model files.
/// If this is set to `1`, it will instead use `Rbxmx`. If it is set to `2`,
/// it will use `JsonModel`.
///
/// This will **not** override existing `Rbxm` middleware. It will only impact
/// new files.
const DEBUG_MODEL_FORMAT_VAR: &str = "ROJO_SYNCBACK_DEBUG";

/// A glob that can be used to tell if a path contains a `.git` folder.
static GIT_IGNORE_GLOB: OnceLock<Glob> = OnceLock::new();

/// Serializes one file-backed instance without rewriting its descendants.
pub fn syncback_instance(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    id: Ref,
    new_tree: WeakDom,
) -> anyhow::Result<FsSnapshot> {
    syncback_scoped(vfs, old_tree, project, id, new_tree, false, None)
}

type ProjectDeclarationBytes = HashMap<PathBuf, Option<Vec<u8>>>;

/// A live Script's suffix change, planned from the accepted property overlay.
/// The caller must install its output in one recoverable replacement transaction.
#[derive(Debug)]
pub(crate) struct ScriptSuffixMigration {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub middleware: Middleware,
    pub ownership: ScriptMigrationOwnership,
}

/// Resolve missing tails using the backend's namespace. A listed but unresolved
/// entry, uncertain case/Unicode equivalence, or an I/O error cannot prove absence.
/// Watcher dependency proofs and migration ownership must use the same identity.
pub(crate) fn canonical_dependency_path(
    fs: &mut memofs::VfsLock<'_>,
    path: &Path,
) -> Option<PathBuf> {
    if !fs.is_absolute_path(path) {
        return None;
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs.normalize(ancestor) {
            Ok(mut resolved) => {
                for name in missing.into_iter().rev() {
                    resolved.push(name);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()?
                    .to_str()
                    .filter(|name| name.is_ascii())?;
                let parent = ancestor.parent()?;
                if let Some(entries) = fs.read_dir(parent).with_not_found().ok()? {
                    for entry in entries {
                        let entry = entry.ok()?;
                        let candidate = entry.path().file_name()?.to_str()?;
                        if candidate.eq_ignore_ascii_case(name) {
                            return None;
                        }
                        // The backend already reported the ASCII target missing. Only
                        // a resolved Unicode sibling proves a distinct namespace entry.
                        if !candidate.is_ascii() && fs.normalize(entry.path()).is_err() {
                            return None;
                        }
                    }
                }
                missing.push(name);
                ancestor = parent;
            }
            Err(_) => return None,
        }
    }
}

/// Read-only identity evidence survives commit and crash recovery, including
/// aliases used by declarations whose optional destinations do not exist yet.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct ScriptMigrationOwnership {
    pub declarations: ProjectDeclarationBytes,
    paths: HashMap<PathBuf, PathBuf>,
    directories: HashMap<PathBuf, Vec<PathBuf>>,
    files: HashSet<PathBuf>,
    absent: HashSet<PathBuf>,
    // Declaration traversal sees siblings too. Only the scoped serializer's exact
    // output may grant write authority beyond the project directory after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    write_authority: Option<ScriptMigrationWriteAuthority>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScriptMigrationWriteAuthority {
    source: PathBuf,
    destination: PathBuf,
    paths: HashSet<PathBuf>,
}

impl ScriptMigrationOwnership {
    pub(crate) fn validate(
        &self,
        fs: &mut memofs::VfsLock<'_>,
        writes: &[PathBuf],
    ) -> std::io::Result<()> {
        // This indexes frozen authority, never observations. Older journals without
        // every lexical parent keep the exhaustive loop; validation cannot invent
        // recovery authority by normalizing a parent that was not captured.
        let write_parents = (|| {
            let mut index: HashMap<&Path, Vec<&PathBuf>> = HashMap::new();
            for write in writes {
                let canonical = self.paths.get(write.parent()?)?;
                index.entry(canonical.as_path()).or_default().push(write);
            }
            Some(index)
        })();
        if write_parents.is_some() {
            self.validate_write_parents(fs, writes)?;
        }
        for (path, expected) in &self.declarations {
            if fs
                .read(path)
                .with_not_found()?
                .as_deref()
                .map(Vec::as_slice)
                != expected.as_deref()
            {
                return Err(std::io::Error::other(format!(
                    "Project declaration changed at {}",
                    path.display()
                )));
            }
        }
        for (path, expected) in &self.paths {
            if canonical_dependency_path(fs, path).as_ref() != Some(expected) {
                return Err(std::io::Error::other(format!(
                    "Project mapping identity changed at {}",
                    path.display()
                )));
            }
        }
        for path in &self.files {
            if !writes.contains(path) && !fs.metadata(path)?.is_file() {
                return Err(std::io::Error::other(format!(
                    "Project dependency is no longer a file at {}",
                    path.display()
                )));
            }
        }
        for path in &self.absent {
            if fs.metadata(path).with_not_found()?.is_some() {
                return Err(std::io::Error::other(format!(
                    "Project mapping appeared at {}",
                    path.display()
                )));
            }
        }
        for (directory, expected) in &self.directories {
            let canonical = fs.normalize(directory)?;
            let mut allowed = HashSet::new();
            let mut allow_write = |write: &Path| {
                if write.parent().is_some_and(|parent| {
                    fs.normalize(parent).is_ok_and(|parent| parent == canonical)
                }) {
                    if let Some(name) = write.file_name() {
                        allowed.insert(PathBuf::from(name));
                    }
                }
            };
            if let Some(index) = &write_parents {
                // Keep each relevant observation in writes order, including repeated
                // parents. A transient retarget after normalize(directory) must not
                // be hidden by before/after guards or a reused successful result.
                if let Some(candidates) = index.get(canonical.as_path()) {
                    for write in candidates {
                        allow_write(write);
                    }
                }
            } else {
                for write in writes {
                    allow_write(write);
                }
            }
            let mut actual = fs
                .read_dir(directory)?
                .map(|entry| entry.map(|entry| PathBuf::from(entry.path().file_name().unwrap())))
                .collect::<std::io::Result<Vec<_>>>()?;
            actual.retain(|name| !allowed.contains(name));
            actual.sort();
            let expected: Vec<_> = expected
                .iter()
                .filter(|name| !allowed.contains(*name))
                .cloned()
                .collect();
            if actual != expected {
                return Err(std::io::Error::other(format!(
                    "Project declaration directory changed at {}",
                    directory.display()
                )));
            }
        }
        if write_parents.is_some() {
            self.validate_write_parents(fs, writes)?;
        }
        Ok(())
    }

    fn validate_write_parents(
        &self,
        fs: &mut memofs::VfsLock<'_>,
        writes: &[PathBuf],
    ) -> std::io::Result<()> {
        for write in writes {
            // Called only after all lexical parents have frozen path evidence.
            let parent = write.parent().unwrap();
            if fs.normalize(parent)?.as_path() != self.paths[parent].as_path() {
                return Err(std::io::Error::other(format!(
                    "Script write parent identity changed at {}",
                    parent.display()
                )));
            }
        }
        Ok(())
    }

    fn identity(&mut self, vfs: &Vfs, path: &Path) -> anyhow::Result<PathBuf> {
        let canonical = canonical_dependency_path(&mut vfs.lock(), path).with_context(|| {
            format!(
                "Cannot prove project mapping identity at {}",
                path.display()
            )
        })?;
        if let Some(previous) = self.paths.insert(path.to_path_buf(), canonical.clone()) {
            anyhow::ensure!(previous == canonical, "Project mapping identity changed");
        }
        Ok(canonical)
    }

    fn capture(&mut self, vfs: &Vfs, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        self.identity(vfs, path)?;
        if !self.declarations.contains_key(path) {
            self.declarations.insert(
                path.to_path_buf(),
                vfs.read(path).with_not_found()?.map(|bytes| bytes.to_vec()),
            );
        }
        Ok(self.declarations[path].clone())
    }

    fn project(&mut self, vfs: &Vfs, path: &Path, name: Option<&str>) -> anyhow::Result<Project> {
        let bytes = self
            .capture(vfs, path)?
            .context("A project declaration disappeared while planning a script migration")?;
        let frozen = Vfs::new(memofs::InMemoryFs::new());
        frozen.create_dir_all(path.parent().context("Project path has no parent")?)?;
        frozen.write(path, bytes)?;
        Ok(Project::load_exact(&frozen, path, name)?)
    }
}

/// Walk declaration references even when their optional roots produce no instance.
/// Filesystem traversal reads directory entries and declarations only, never payloads.
struct MigrationDeclarations<'a> {
    vfs: &'a Vfs,
    targets: Vec<PathBuf>,
    ownership: ScriptMigrationOwnership,
    active_projects: HashSet<PathBuf>,
    active_directories: HashSet<PathBuf>,
    served_ancestry: Option<HashSet<PathBuf>>,
    source_views: usize,
}

impl MigrationDeclarations<'_> {
    fn project(
        &mut self,
        context: &crate::snapshot::InstanceContext,
        project: &Project,
    ) -> anyhow::Result<()> {
        let canonical = self.ownership.identity(self.vfs, &project.file_location)?;
        anyhow::ensure!(
            self.active_projects.insert(canonical.clone()),
            "Project dependency cycle prevents script migration"
        );
        let mut context = context.clone();
        context.clear_sync_rules();
        context.add_sync_rules(
            project
                .sync_rules
                .iter()
                .map(|rule| crate::snapshot::SyncRule {
                    base_path: project.folder_location().to_path_buf(),
                    ..rule.clone()
                }),
        );
        context.add_path_ignore_rules(project.glob_ignore_paths.iter().map(|glob| {
            crate::snapshot::PathIgnoreRule {
                glob: glob.clone(),
                base_path: project.folder_location().to_path_buf(),
            }
        }));
        // Match project middleware's branch-local reset without weakening project cycles.
        let directories = std::mem::take(&mut self.active_directories);
        self.node(&context, project, &project.tree)?;
        self.active_directories = directories;
        self.active_projects.remove(&canonical);
        Ok(())
    }

    fn node(
        &mut self,
        context: &crate::snapshot::InstanceContext,
        project: &Project,
        node: &crate::project::ProjectNode,
    ) -> anyhow::Result<()> {
        if let Some(mapping) = &node.path {
            let path = project.folder_location().join(mapping.path());
            let canonical = self.ownership.identity(self.vfs, &path)?;
            self.filesystem(context, &canonical, true)?;
        }
        for child in node.children.values() {
            self.node(context, project, child)?;
        }
        Ok(())
    }

    fn filesystem(
        &mut self,
        context: &crate::snapshot::InstanceContext,
        path: &Path,
        reservation: bool,
    ) -> anyhow::Result<()> {
        let mut context = context.clone();
        let mut path = path.to_path_buf();
        if let Some((root, _)) = &context.package_mount {
            crate::package_sources::contained(self.vfs, root, &path)?;
        }
        if let Some(packages) = &context.package_sources {
            if path == packages.root.join("wally.install.json") {
                return Ok(());
            }
            if let Some(package) = packages.local_mount(&path) {
                packages.check_generation(self.vfs)?;
                context.package_mount = Some((
                    packages.root.join(package.source_path.as_ref().unwrap()),
                    packages.root.join(&package.install_path),
                ));
                path = packages.original_path(&crate::package_sources::normalize(&path));
                return self.filesystem(&context, &path, reservation);
            }
        }
        if context.is_reserved_path(&path) {
            return Ok(());
        }
        if reservation {
            let canonical = self.ownership.identity(self.vfs, &path)?;
            anyhow::ensure!(
                !self.targets.contains(&canonical),
                "A project mapping pins this script source"
            );
        }
        let Some(metadata) = self.vfs.metadata(&path).with_not_found()? else {
            self.ownership.absent.insert(path.clone());
            // Missing custom Project-rule files are probes too, not only *.project.json.
            if crate::change_processor::dependency_file_middleware(&context, &path)
                == Some(Middleware::Project)
            {
                self.ownership.capture(self.vfs, &path)?;
            }
            return Ok(());
        };
        if metadata.is_file() {
            self.ownership.files.insert(path.clone());
            let canonical = self.ownership.identity(self.vfs, &path)?;
            if self.targets.first() == Some(&canonical) {
                self.source_views += 1;
                anyhow::ensure!(
                    self.source_views == 1,
                    "A project mapping introduces another owner for this script source"
                );
                if let Some(served) = &self.served_ancestry {
                    anyhow::ensure!(
                        served == &self.active_projects,
                        "The script's served project ancestry changed before migration"
                    );
                }
            }
        }
        if metadata.is_dir() {
            let canonical = self.ownership.identity(self.vfs, &path)?;
            anyhow::ensure!(
                self.active_directories.insert(canonical.clone()),
                "Directory dependency cycle prevents script migration"
            );
            let mut selected = None;
            for name in DEFAULT_PROJECT_NAMES {
                let probe = path.join(name);
                if self.ownership.capture(self.vfs, &probe)?.is_some() && selected.is_none() {
                    selected = Some(probe);
                }
            }
            if let Some(project_path) = selected {
                let project = self
                    .ownership
                    .project(self.vfs, &project_path, Some("Migration"))?;
                self.project(&context, &project)?;
            } else if self
                .targets
                .iter()
                .any(|target| target.starts_with(&canonical))
            {
                // Only directories on the way to a target can hold it or a project
                // file that shadows it, so other subtrees are probed but not walked.
                // Walking the whole project made every rename scale with project size.
                let entries = self
                    .vfs
                    .read_dir(&path)?
                    .collect::<std::io::Result<Vec<_>>>()?;
                let mut names: Vec<_> = entries
                    .iter()
                    .map(|entry| PathBuf::from(entry.path().file_name().unwrap()))
                    .collect();
                names.sort();
                if let Some(previous) = self
                    .ownership
                    .directories
                    .insert(path.clone(), names.clone())
                {
                    anyhow::ensure!(previous == names, "Project declaration directory changed");
                }
                for entry in entries {
                    if !crate::snapshot::is_path_ignored(&context.path_ignore_rules, entry.path()) {
                        self.filesystem(&context, entry.path(), false)?;
                    }
                }
            }
            self.active_directories.remove(&canonical);
        } else if !dir_init_files()
            .iter()
            .any(|(_, name)| path.file_name().is_some_and(|file| file == *name))
            && crate::change_processor::dependency_file_middleware(&context, &path)
                == Some(Middleware::Project)
        {
            let project = self.ownership.project(self.vfs, &path, Some("Migration"))?;
            self.project(&context, &project)?;
        }
        Ok(())
    }
}

fn script_migration_projects(
    vfs: &Vfs,
    context: &crate::snapshot::InstanceContext,
    project: &Project,
    targets: &[&Path],
) -> anyhow::Result<ScriptMigrationOwnership> {
    let mut graph = MigrationDeclarations {
        vfs,
        targets: Vec::new(),
        ownership: ScriptMigrationOwnership::default(),
        active_projects: HashSet::new(),
        active_directories: HashSet::new(),
        served_ancestry: (!context.served_projects.0.is_empty()).then(HashSet::new),
        source_views: 0,
    };
    for path in targets {
        graph.targets.push(graph.ownership.identity(vfs, path)?);
    }
    for served in context.served_projects.0.iter() {
        let canonical = graph.ownership.identity(vfs, &served.file_location)?;
        graph.served_ancestry.as_mut().unwrap().insert(canonical);
    }
    // Frozen current bytes are not proof that they served the live instance.
    for served in context
        .served_projects
        .0
        .iter()
        .map(AsRef::as_ref)
        .chain([project])
    {
        let current =
            graph
                .ownership
                .project(vfs, &served.file_location, served.name.as_deref())?;
        anyhow::ensure!(
            current == *served,
            "The served project changed while planning a script migration"
        );
    }
    let mut root_context = crate::snapshot::InstanceContext::new();
    root_context.package_sources = context.package_sources.clone();
    if let Some(cache) = context.reserved_cache_path() {
        root_context.set_reserved_cache_path(cache.to_path_buf());
    }
    graph.project(&root_context, project)?;
    anyhow::ensure!(
        graph.served_ancestry.is_none() || graph.source_views == 1,
        "The served script source is no longer owned by its project declaration graph"
    );
    graph.ownership.validate(&mut vfs.lock(), &[])?;
    Ok(graph.ownership)
}

pub(crate) fn syncback_script_migration(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    id: Ref,
    new_tree: WeakDom,
) -> anyhow::Result<(FsSnapshot, Option<ScriptSuffixMigration>)> {
    let old = old_tree
        .get_instance(id)
        .context("Unknown script instance")?;
    let mut migration = (|| {
        if old.class_name() != "Script" || new_tree.root().class != "Script" {
            return None;
        }
        let script = script_middleware_for_syncback(new_tree.root(), Some(old), project)?;
        let source = source_file(&old)?;
        let canonical = file_names::canonical_script_source_name(
            script,
            &source,
            &old.metadata().context.sync_rules,
        )?;
        let destination = source.with_file_name(canonical.as_ref());
        (source != destination).then(|| ScriptSuffixMigration {
            source,
            destination,
            middleware: if old
                .metadata()
                .middleware
                .is_some_and(|value| value.is_dir())
            {
                file_names::directory_variant(script)
            } else {
                script
            },
            ownership: ScriptMigrationOwnership::default(),
        })
    })();
    if let Some(migration) = &mut migration {
        let path = old.metadata().instigating_source.as_ref().unwrap().path();
        migration.ownership = script_migration_projects(
            vfs,
            &old.metadata().context,
            project,
            &[&migration.source, &migration.destination, path],
        )?;
    }
    let output = syncback_scoped(
        vfs,
        old_tree,
        project,
        id,
        new_tree,
        false,
        migration.as_ref(),
    )?;
    if let Some(migration) = &mut migration {
        for path in [&migration.source, &migration.destination] {
            migration.ownership.identity(
                vfs,
                path.parent()
                    .context("Script migration path has no parent")?,
            )?;
        }
        let mut paths = HashSet::new();
        for (path, _) in output.file_changes() {
            let path = project.folder_location().join(path);
            migration.ownership.identity(vfs, &path)?;
            // Capture the lexical parent separately. Its canonical prefix spelling
            // need not match the prefix selected when canonicalizing the file.
            migration.ownership.identity(
                vfs,
                path.parent()
                    .context("Script migration output has no parent")?,
            )?;
            paths.insert(path);
        }
        migration.ownership.write_authority = Some(ScriptMigrationWriteAuthority {
            source: migration.source.clone(),
            destination: migration.destination.clone(),
            paths,
        });
    }
    Ok((output, migration))
}

/// Serializes a model file from `new_tree`, which must hold the model's whole
/// subtree, since the file stores every descendant.
pub fn syncback_model_file(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    id: Ref,
    new_tree: WeakDom,
) -> anyhow::Result<FsSnapshot> {
    syncback_scoped(vfs, old_tree, project, id, new_tree, true, None)
}

/// Links the references between instances in `dom` through `Rojo_Id` and
/// `Rojo_Target_*` attributes, like a full syncback does, unless the project
/// opts out. Instances keep the IDs they already carry. References to
/// instances outside `dom` can't be linked and become nil.
pub(crate) fn link_scoped_referents(project: &Project, dom: &mut WeakDom) -> anyhow::Result<()> {
    let ignore_referents = project
        .syncback_rules
        .as_ref()
        .and_then(|rules| rules.ignore_referents)
        .unwrap_or_default();
    if ignore_referents {
        return Ok(());
    }
    // Linking rewrites every `Rojo_Target_*` attribute of an instance that
    // points inside `dom`. One whose property was cleared must go, and one
    // that points outside `dom`, held by the attribute alone, must stay.
    let mut kept = Vec::new();
    let mut cleared = Vec::new();
    for referent in descendants(dom, dom.root_ref()) {
        let instance = dom.get_by_ref(referent).unwrap();
        let Some(Variant::Attributes(attributes)) = instance.properties.get(&ustr("Attributes"))
        else {
            continue;
        };
        for (name, value) in attributes.iter() {
            let Some(property) = name.strip_prefix(REF_POINTER_ATTRIBUTE_PREFIX) else {
                continue;
            };
            match instance.properties.get(&ustr(property)) {
                Some(Variant::Ref(target)) if target.is_none() => {
                    cleared.push((referent, name.clone()))
                }
                Some(Variant::Ref(target)) if dom.get_by_ref(*target).is_some() => {}
                _ => kept.push((referent, name.clone(), value.clone())),
            }
        }
    }
    for (referent, name) in cleared {
        if let Some(Variant::Attributes(attributes)) = dom
            .get_by_ref_mut(referent)
            .and_then(|instance| instance.properties.get_mut(&ustr("Attributes")))
        {
            attributes.remove(name.as_str());
        }
    }
    let links = collect_referents(dom);
    link_referents(links, dom, &HashMap::new())?;
    for (referent, name, value) in kept {
        if let Some(Variant::Attributes(attributes)) = dom
            .get_by_ref_mut(referent)
            .and_then(|instance| instance.properties.get_mut(&ustr("Attributes")))
        {
            attributes.insert(name, value);
        }
    }
    Ok(())
}

fn syncback_scoped(
    vfs: &Vfs,
    old_tree: &RojoTree,
    project: &Project,
    id: Ref,
    mut new_tree: WeakDom,
    whole_model: bool,
    migration: Option<&ScriptSuffixMigration>,
) -> anyhow::Result<FsSnapshot> {
    let old_inst = old_tree
        .get_instance(id)
        .context("Cannot sync an unknown instance")?;
    let source = old_inst
        .metadata()
        .instigating_source
        .as_ref()
        .context("The instance has no filesystem source")?;
    let path = source.path().to_path_buf();
    let middleware = old_inst
        .metadata()
        .middleware
        .context("The instance has no known filesystem middleware")?;
    let is_model = matches!(
        middleware,
        Middleware::Rbxm | Middleware::Rbxmx | Middleware::JsonModel
    );
    if is_model != whole_model {
        anyhow::bail!(if is_model {
            "Model-file updates require subtree serialization"
        } else {
            "Only model files are serialized with their whole subtree"
        });
    }
    // Binary models hold references natively. Other formats need attributes.
    if !matches!(middleware, Middleware::Rbxm | Middleware::Rbxmx) {
        link_scoped_referents(project, &mut new_tree)?;
    }
    if let (Some(script), Some(source)) = (
        script_middleware_for_syncback(new_tree.root(), Some(old_inst), project),
        source_file(&old_inst),
    ) {
        if let Some(name) = source.file_name().and_then(|name| name.to_str()) {
            if migration.is_none()
                && file_names::canonical_script_source_name(
                    script,
                    &source,
                    &old_inst.metadata().context.sync_rules,
                )
                .is_some_and(|canonical| canonical != name)
            {
                anyhow::bail!(
                    "This script's class or RunContext requires a different source filename. \
                     Run a full syncback to safely update the script suffix."
                );
            }
        }
    }
    let attribute_ids = attribute_ids(&new_tree);
    let snapshot = SyncbackSnapshot {
        data: SyncbackData {
            vfs,
            old_tree,
            new_tree: &new_tree,
            project,
            scoped: true,
            context: None,
            script_containers: None,
            model_trees: None,
            attribute_ids: &attribute_ids,
        },
        old: Some(id),
        new: new_tree.root_ref(),
        path: migration
            .filter(|_| !middleware.is_dir())
            .map_or_else(|| path.clone(), |migration| migration.destination.clone()),
        middleware: migration.is_none().then_some(middleware),
    };
    if let Some(migration) = migration {
        anyhow::ensure!(
            !is_project_owned(old_inst.metadata()),
            "Fixed project mappings cannot migrate script suffixes"
        );
        for target in [&migration.source, &migration.destination] {
            anyhow::ensure!(
                !crate::snapshot::is_path_ignored(
                    &old_inst.metadata().context.path_ignore_rules,
                    target
                ),
                "The script migration source or destination is ignored"
            );
            anyhow::ensure!(
                old_tree
                    .get_ids_at_path(target)
                    .iter()
                    .all(|owner| *owner == id),
                "An aliased script source cannot migrate"
            );
        }
        snapshot.validate_script_migration(&migration.source, &migration.destination)?;
        snapshot.validate_script_destination(&migration.destination)?;
        snapshot.validate_script_rule_for_middleware(
            &migration.destination,
            file_names::script_middleware_for_instance(new_tree.root()).unwrap(),
        )?;
        if middleware.is_dir() {
            for (_, initializer) in dir_init_files() {
                let candidate = path.join(initializer);
                anyhow::ensure!(
                    candidate == migration.source
                        || vfs.metadata(&candidate).with_not_found()?.is_none(),
                    "A competing initializer prevents script suffix migration"
                );
            }
        }
    }
    if let Some(rules) = &project.syncback_rules {
        let globs = rules.compile_globs()?;
        if !is_valid_path(&Some(globs), project.folder_location(), &path) {
            anyhow::bail!("This path is excluded by the project's syncback rules");
        }
        let instance_path = snapshot.get_old_inst_path(id);
        if rules
            .ignore_trees
            .iter()
            .any(|ignored| instance_path.starts_with(ignored.as_str()))
        {
            anyhow::bail!("This instance is excluded by the project's syncback rules");
        }
    }
    let mut output = get_best_middleware(&snapshot)
        .syncback(&snapshot)?
        .fs_snapshot;

    // Directory scripts are serialized to the default initializer name, but
    // the directory may use another one, like `init.lua`. Writing both would
    // be a multi-file change and leave two competing initializers.
    if let Some(tracked) = source_file(&old_inst).filter(|tracked| *tracked != path) {
        let tracked = migration.map_or(tracked, |migration| migration.destination.clone());
        for (_, name) in dir_init_files() {
            let staged = path.join(name);
            if staged != tracked {
                output.retarget_added_file(&staged, &tracked);
            }
        }
    }

    // These match the paths the metadata serializers write to, which are based
    // on the file stem (`Foo.meta.json` for `Foo.server.luau`).
    let (json_path, jsonc_path) = if middleware.is_dir() {
        (path.join("init.meta.json"), path.join("init.meta.jsonc"))
    } else {
        let parent = path
            .parent()
            .context("The instance's source has no parent directory")?;
        let stem = snapshot.file_stem();
        (
            parent.join(format!("{stem}.meta.json")),
            parent.join(format!("{stem}.meta.jsonc")),
        )
    };
    let is_file = |path: &Path| -> anyhow::Result<bool> {
        Ok(vfs
            .metadata(path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_file()))
    };
    if is_file(&jsonc_path)? {
        // Both files are applied when reading, so rewriting only one of them
        // can't clear a value the other still sets.
        if is_file(&json_path)? {
            anyhow::bail!(
                "Both {} and {} hold this instance's metadata, so it can't be safely rewritten",
                json_path.display(),
                jsonc_path.display()
            );
        }
        output.retarget_added_file(&json_path, &jsonc_path);
    }
    if let Some(migration) = migration {
        output.remove_file(&migration.source);
    }
    output.guard_packages(vfs, old_inst.metadata().context.package_sources.as_deref());
    Ok(output)
}

/// Returns an error if a staged scoped write would add, change, or remove a path
/// the project's syncback rules exclude. Run this on the final write targets, after
/// retargeting and filtering, since those decide which files actually change.
pub fn validate_write_targets(project: &Project, staged: &FsSnapshot) -> anyhow::Result<()> {
    let Some(rules) = &project.syncback_rules else {
        return Ok(());
    };
    let globs = Some(rules.compile_globs()?);
    let base = project.folder_location();
    let excluded = staged
        .added_paths()
        .into_iter()
        .chain(staged.removed_paths())
        .find(|path| !is_valid_path(&globs, base, path));
    match excluded {
        Some(path) => anyhow::bail!(
            "{} is excluded by the project's syncback rules",
            path.display()
        ),
        None => Ok(()),
    }
}

/// Returns the file holding an instance's own source, which for a directory
/// script is its initializer (`init.luau`, `init.server.lua`, ...). Returns
/// `None` for plain directories, which have no source file.
pub fn source_file(instance: &InstanceWithMeta) -> Option<PathBuf> {
    let metadata = instance.metadata();
    let path = metadata.instigating_source.as_ref()?.path();
    match metadata.middleware? {
        Middleware::Dir => None,
        middleware if middleware.is_dir() => metadata
            .relevant_paths
            .iter()
            .find(|candidate| {
                candidate.parent() == Some(path)
                    && candidate
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| dir_init_files().iter().any(|(_, init)| *init == name))
            })
            .cloned(),
        _ => Some(path.to_path_buf()),
    }
}

/// Package edits never use Studio authority to overwrite an unseen source edit.
/// Return a warning before catch-up can publish a restoration to Studio.
pub(crate) fn package_source_conflict(vfs: &Vfs, tree: &RojoTree, id: Ref) -> anyhow::Result<bool> {
    let Some(instance) = tree.get_instance(id) else {
        return Ok(false);
    };
    for (path, expected) in &instance.metadata().package_file_contents {
        let current = vfs.read(path).with_not_found()?;
        let baseline = expected
            .as_ref()
            .map(|bytes| Variant::BinaryString(bytes.clone().into()));
        let filesystem = current.map(|bytes| Variant::BinaryString(bytes.to_vec().into()));
        if decide_property_write(baseline.as_ref(), filesystem.as_ref(), baseline.as_ref())
            == PropertyWriteDecision::RestoreFilesystem
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn syncback_loop(
    vfs: &Vfs,
    old_tree: &mut RojoTree,
    mut new_tree: WeakDom,
    project: &Project,
) -> anyhow::Result<FsSnapshot> {
    let package_paths: Vec<_> = old_tree
        .inner()
        .descendants()
        .filter_map(|instance| {
            let instance = old_tree.get_instance(instance.referent())?;
            let packages = instance.metadata().context.package_sources.as_ref()?;
            Some(
                instance
                    .metadata()
                    .relevant_paths
                    .iter()
                    .filter(|path| packages.is_local_source(path))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();
    for instance in old_tree.inner().descendants() {
        if package_source_conflict(vfs, old_tree, instance.referent())? {
            anyhow::bail!(
                "Package source changed concurrently for {}. Preserving filesystem and Studio edits without writing the batch",
                instance.referent()
            );
        }
    }
    let package_expected = FsSnapshot::capture_file_contents(&package_paths, vfs)?;
    let ignore_patterns = project
        .syncback_rules
        .as_ref()
        .map(|rules| rules.compile_globs())
        .transpose()?;

    // TODO: Add a better way to tell if the root of a project is a directory
    let skip_pruning = if let Some(path) = &project.tree.path {
        let middleware =
            Middleware::middleware_for_path(vfs, &project.sync_rules, path.path()).unwrap();
        if let Some(middleware) = middleware {
            middleware.is_dir()
        } else {
            false
        }
    } else {
        false
    };
    if !skip_pruning {
        // Strip out any objects from the new tree that aren't in the old tree. This
        // is necessary so that hashing the roots of each tree won't always result
        // in different hashes. Shout out to Roblox for serializing a bunch of
        // Services nobody cares about.
        log::debug!("Pruning new tree");
        strip_unknown_root_children(&mut new_tree, old_tree);
    }

    log::debug!("Collecting referents for new DOM...");
    let deferred_referents = collect_referents(&new_tree);
    let ids_on_disk = ids_on_disk(&new_tree, old_tree.inner());

    // Remove any properties that are manually blocked from syncback via the
    // project file.
    log::debug!("Pre-filtering properties on DOMs");
    for referent in descendants(&new_tree, new_tree.root_ref()) {
        let new_inst = new_tree.get_by_ref_mut(referent).unwrap();
        if let Some(filter) = get_property_filter(project, new_inst) {
            for prop in filter {
                new_inst.properties.remove(&prop);
            }
        }
    }
    for referent in descendants(old_tree.inner(), old_tree.get_root_id()) {
        let mut old_inst_rojo = old_tree.get_instance_mut(referent).unwrap();
        let old_inst = old_inst_rojo.inner_mut();
        if let Some(filter) = get_property_filter(project, old_inst) {
            for prop in filter {
                old_inst.properties.remove(&prop);
            }
        }
    }

    // Handle removing the current camera.
    if let Some(syncback_rules) = &project.syncback_rules {
        if !syncback_rules.sync_current_camera.unwrap_or_default() {
            log::debug!("Removing CurrentCamera from new DOM");
            let mut camera_ref = None;
            for child_ref in new_tree.root().children() {
                let inst = new_tree.get_by_ref(*child_ref).unwrap();
                if inst.class == "Workspace" {
                    camera_ref = inst.properties.get(&ustr("CurrentCamera"));
                    break;
                }
            }
            if let Some(Variant::Ref(camera_ref)) = camera_ref {
                if new_tree.get_by_ref(*camera_ref).is_some() {
                    new_tree.destroy(*camera_ref);
                }
            }
        }
    }

    let ignore_referents = project
        .syncback_rules
        .as_ref()
        .and_then(|s| s.ignore_referents)
        .unwrap_or_default();
    if !ignore_referents {
        log::debug!("Linking referents for new DOM");
        link_referents(deferred_referents, &mut new_tree, &ids_on_disk)?;
    } else {
        log::debug!("Skipping referent linking as per project syncback rules");
    }

    // As with pruning the children of the new root, we need to ensure the roots
    // for both DOMs have the same name otherwise their hashes will always be
    // different.
    new_tree.root_mut().name = old_tree.root().name().to_string();

    log::debug!("Hashing project DOM");
    let old_hashes = hash_tree(project, old_tree.inner(), old_tree.get_root_id());
    log::debug!("Hashing file DOM");
    let new_hashes = hash_tree(project, &new_tree, new_tree.root_ref());

    let model_tree_rules = model_trees::CompiledModelTreeRules::new(
        project
            .syncback_rules
            .as_ref()
            .map(|rules| rules.model_trees.as_slice())
            .unwrap_or_default(),
        &project.tree,
    )?;
    let mut relayout = if instances_as_directories(project) {
        inline::needs_relayout(
            old_tree,
            &ModelTrees::find(&model_tree_rules, old_tree.inner()),
        )
    } else {
        HashSet::new()
    };
    relayout.extend(model_trees::needs_relayout(
        &model_tree_rules,
        old_tree,
        instances_as_directories(project),
    ));
    for instance in old_tree.inner().descendants() {
        let old = old_tree.get_instance(instance.referent()).unwrap();
        if !matches!(
            old.metadata().instigating_source,
            Some(crate::snapshot::InstigatingSource::Path(_))
        ) {
            continue;
        }
        let Some(middleware) = script_middleware_for_syncback(instance, Some(old), project) else {
            continue;
        };
        let Some(source) = source_file(&old) else {
            continue;
        };
        let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_names::canonical_script_source_name(
            middleware,
            &source,
            &old.metadata().context.sync_rules,
        )
        .is_some_and(|canonical| canonical != name)
        {
            let mut current = old.id();
            while relayout.insert(current) {
                match old_tree.get_instance(current) {
                    Some(ancestor) if ancestor.parent().is_some() => current = ancestor.parent(),
                    _ => break,
                }
            }
        }
    }
    let model_trees = ModelTrees::find(&model_tree_rules, &new_tree);
    let script_containers = inline::script_containers(&new_tree);
    let attribute_ids = attribute_ids(&new_tree);
    let syncback_data = SyncbackData {
        vfs,
        old_tree,
        new_tree: &new_tree,
        project,
        scoped: false,
        context: None,
        script_containers: Some(&script_containers),
        model_trees: Some(&model_trees),
        attribute_ids: &attribute_ids,
    };

    let mut snapshots = vec![SyncbackSnapshot {
        data: syncback_data,
        old: Some(old_tree.get_root_id()),
        new: new_tree.root_ref(),
        path: project.file_location.clone(),
        middleware: Some(Middleware::Project),
    }];

    let mut fs_snapshot = FsSnapshot::new();
    let mut file_owners = HashMap::new();

    while let Some(snapshot) = snapshots.pop() {
        let inst_path = snapshot.get_new_inst_path(snapshot.new);
        // We can quickly check that two subtrees are identical and if they are,
        // skip reconciling them.
        // Unchanged content can still be in an outdated layout, so those
        // subtrees are reconciled to move files into their new places.
        if let Some(old_ref) = snapshot.old.filter(|old| !relayout.contains(old)) {
            match (old_hashes.get(&old_ref), new_hashes.get(&snapshot.new)) {
                (Some(old), Some(new)) => {
                    if old == new {
                        log::trace!(
                            "Skipping {inst_path} due to it being identically hashed as {old:?}"
                        );
                        continue;
                    }
                }
                _ => unreachable!("All Instances in both DOMs should have hashes"),
            }
        }

        if let Some((children, _)) = syncback_snapshot(
            snapshot,
            &ignore_patterns,
            project,
            &relayout,
            &mut fs_snapshot,
            &mut file_owners,
        )? {
            snapshots.extend(children);
        }
    }

    fs_snapshot.guard_packages(
        vfs,
        old_tree
            .root()
            .metadata()
            .context
            .package_sources
            .as_deref(),
    );
    fs_snapshot.package_expected = package_expected;
    Ok(fs_snapshot)
}

/// Serializes one instance into `fs_snapshot` and returns the snapshots for its
/// children, along with the path the instance was written to. Returns `None`
/// when the project's rules leave the instance out, or a model file already
/// holds exactly this subtree. Instances in `relayout` are always rewritten.
fn syncback_snapshot<'sync>(
    mut snapshot: SyncbackSnapshot<'sync>,
    ignore_patterns: &Option<Vec<IgnorableGlob>>,
    project: &Project,
    relayout: &HashSet<Ref>,
    fs_snapshot: &mut FsSnapshot,
    file_owners: &mut HashMap<PathBuf, Ref>,
) -> anyhow::Result<Option<(Vec<SyncbackSnapshot<'sync>>, PathBuf)>> {
    if let Some(old) = snapshot
        .old_inst()
        .filter(|old| old.metadata().context.package_mount.is_some())
    {
        let metadata = old.metadata();
        let physical = if get_best_middleware(&snapshot) == Middleware::Project {
            // Referenced projects share metadata with their outer project. Keep
            // the original inner project (or its directory) selected by
            // syncback_project, but still redirect installed entrypoints to the
            // authoritative source.
            if ((Project::is_project_file(&snapshot.path)
                && metadata.relevant_paths.contains(&snapshot.path))
                || DEFAULT_PROJECT_NAMES
                    .iter()
                    .any(|name| metadata.relevant_paths.contains(&snapshot.path.join(name))))
                && metadata
                    .context
                    .package_mount
                    .as_ref()
                    .is_some_and(|(source, _)| snapshot.path.starts_with(source))
            {
                Some(&snapshot.path)
            } else {
                metadata
                    .relevant_paths
                    .iter()
                    .rev()
                    .find(|path| Project::is_project_file(path))
            }
        } else {
            metadata.relevant_paths.first()
        };
        if let Some(physical) = physical {
            snapshot.path = physical.clone();
        }
    }
    let inst_path = snapshot.get_new_inst_path(snapshot.new);
    if let Some(packages) = snapshot
        .data
        .old_tree
        .root()
        .metadata()
        .context
        .package_sources
        .as_ref()
    {
        let old = snapshot.old_inst();
        let reason = old
            .as_ref()
            .and_then(|old| {
                old.metadata()
                    .relevant_paths
                    .iter()
                    .find_map(|path| packages.read_only_node(snapshot.vfs(), path))
            })
            .or_else(|| {
                if old.is_none() {
                    packages.blocked(snapshot.vfs(), &snapshot.path, false)
                } else {
                    None
                }
            });
        let reason = reason.or_else(|| {
            if !is_model_file(get_best_middleware(&snapshot)) {
                return None;
            }
            let old = old.as_ref()?;
            snapshot
                .data
                .old_tree
                .descendants(old.id())
                .find_map(|instance| {
                    if instance.metadata().is_package_mount_root() {
                        return Some("A model file cannot absorb a Wally package mount".into());
                    }
                    instance
                        .metadata()
                        .relevant_paths
                        .iter()
                        .find_map(|path| packages.read_only_node(snapshot.vfs(), path))
                })
        });
        if let Some(reason) = reason {
            log::warn!("Skipping Studio package difference at {inst_path}. {reason}. Files and Studio were left unchanged");
            return Ok(None);
        }
    }
    let project_path = project.folder_location();
    let mut written_path = snapshot.path.clone();
    {
        if snapshot.is_script_suffix_migration(&snapshot.path) {
            let old_source = snapshot.old_inst().as_ref().and_then(source_file).unwrap();
            snapshot.validate_script_migration(&old_source, &snapshot.path)?;
        }
        if !is_valid_path(ignore_patterns, project_path, &snapshot.path) {
            log::debug!("Skipping {inst_path} because its path matches ignore pattern");
            return Ok(None);
        }
        if let Some(syncback_rules) = &project.syncback_rules {
            // Ignore trees;
            for ignored in &syncback_rules.ignore_trees {
                if inst_path.starts_with(ignored.as_str()) {
                    log::debug!("Tree {inst_path} is blocked by project");
                    return Ok(None);
                }
            }
        }

        let middleware = get_best_middleware(&snapshot);

        // A model file holds its whole subtree, so it can't leave out a
        // descendant the project asks syncback to ignore.
        if snapshot.model_tree_middleware().is_some() {
            let prefix = format!("{inst_path}/");
            let ignored = project
                .syncback_rules
                .iter()
                .flat_map(|rules| &rules.ignore_trees)
                .find(|tree| tree.starts_with(&prefix));
            if let Some(ignored) = ignored {
                anyhow::bail!(
                    "Cannot store {inst_path} as a model file because the project's \
                     ignoreTrees rule excludes {ignored}"
                );
            }
        }

        // Only a model owns its whole subtree. A directory may mix JSON and
        // binary descendants, so its early comparison must remain exact.
        let model_hasher: Option<fn(&Project, &WeakDom, Ref) -> _> = match middleware {
            Middleware::JsonModel => Some(hash_json_tree),
            Middleware::Rbxm | Middleware::Rbxmx => Some(hash_binary_tree),
            _ => None,
        };
        if let (Some(hash_model), Some(old_ref)) = (
            model_hasher,
            snapshot.old.filter(|old| !relayout.contains(old)),
        ) {
            let old = hash_model(project, snapshot.old_tree(), old_ref);
            let new = hash_model(project, snapshot.new_tree(), snapshot.new);
            if old.get(&old_ref) == new.get(&snapshot.new) {
                return Ok(None);
            }
        }

        log::trace!(
            "Middleware for {inst_path} is {:?} (path is {})",
            middleware,
            snapshot.path.display()
        );

        if matches!(middleware, Middleware::Json | Middleware::Toml) {
            log::warn!("Cannot syncback {middleware:?} at {inst_path}, skipping");
            return Ok(None);
        }

        let syncback = match middleware.syncback(&snapshot) {
            Ok(syncback) => syncback,
            // Writing the directory as a model file instead would remove the
            // very paths an ignore conflict is reported to protect.
            Err(err)
                if middleware == Middleware::Dir
                    && err.downcast_ref::<IgnoreConflict>().is_none() =>
            {
                let new_middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
                    Ok(value) if value == "1" => Middleware::Rbxmx,
                    Ok(value) if value == "2" => Middleware::JsonModel,
                    _ => Middleware::Rbxm,
                };
                let file_name = snapshot
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("Directory middleware should have a name in its path")?;
                let mut path = snapshot.path.clone();
                path.set_file_name(format!(
                    "{file_name}.{}",
                    extension_for_middleware(new_middleware)
                ));
                // The model file is named by its own middleware, not by the
                // directory that could not be written.
                let new_snapshot = snapshot
                    .with_new_path(path, snapshot.new, snapshot.old)
                    .middleware(new_middleware);
                log::warn!(
                    "Could not syncback {inst_path} as a Directory because: {err}.\n\
                    It will instead be synced back as a {new_middleware:?}."
                );
                written_path = new_snapshot.path.clone();
                let new_syncback_result = new_middleware
                    .syncback(&new_snapshot)
                    .with_context(|| format!("Failed to syncback {inst_path}"));
                if new_syncback_result.is_ok() && snapshot.old_inst().is_some() {
                    // We need to remove the old FS representation if we're
                    // reserializing it as an rbxm.
                    fs_snapshot.remove_dir(&snapshot.path);
                }
                new_syncback_result?
            }
            Err(err) => anyhow::bail!("Failed to syncback {inst_path} because {err}"),
        };

        if !syncback.removed_children.is_empty() {
            log::debug!(
                "removed children for {inst_path}: {}",
                syncback.removed_children.len()
            );
            'remove: for inst in &syncback.removed_children {
                if inst.metadata().is_package_mount_root() {
                    log::warn!("Skipping removed local package mount {}. Its ownership is declared by wally.toml and no source files were removed", inst.name());
                    continue;
                }
                // Instances stored inside a parent's metadata have no files of
                // their own and disappear when that metadata is rewritten.
                let Some(source) = inst.metadata().instigating_source.as_ref() else {
                    continue;
                };
                let path = source.path();
                let inst_path = snapshot.get_old_inst_path(inst.id());
                if !is_valid_path(ignore_patterns, project_path, path) {
                    log::debug!(
                        "Skipping removing {} because its matches an ignore pattern",
                        path.display()
                    );
                    continue;
                }
                if let Some(syncback_rules) = &project.syncback_rules {
                    for ignored in &syncback_rules.ignore_trees {
                        if inst_path.starts_with(ignored.as_str()) {
                            log::debug!("Skipping removing {inst_path} because its path is blocked by project");
                            continue 'remove;
                        }
                    }
                }
                if path.is_dir() {
                    fs_snapshot.remove_dir(path)
                } else {
                    fs_snapshot.remove_file(path)
                }
                // The metadata that names a model file's root would otherwise
                // outlive it, since nothing else is written next to it.
                if inst.metadata().middleware.is_some_and(is_model_file) {
                    for meta_path in inst.metadata().relevant_paths.iter().skip(1) {
                        if meta_path.is_file()
                            && is_valid_path(ignore_patterns, project_path, meta_path)
                        {
                            fs_snapshot.remove_file(meta_path)
                        }
                    }
                }
            }
        }

        // TODO provide replacement snapshots for e.g. two way sync

        fs_snapshot.merge_with_filter(
            syncback.fs_snapshot,
            |path| is_valid_path(ignore_patterns, project_path, path),
            snapshot.new,
            file_owners,
        )?;

        Ok(Some((syncback.children, written_path)))
    }
}

/// Reports that a layout the `modelTrees` rules ask for would remove or absorb
/// something the project's ignore rules exclude. Syncback stops on it, where
/// other directory errors make it write the directory as a model file.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct IgnoreConflict(pub String);

pub struct SyncbackReturn<'sync> {
    pub fs_snapshot: FsSnapshot,
    pub children: Vec<SyncbackSnapshot<'sync>>,
    pub removed_children: Vec<InstanceWithMeta<'sync>>,
}

fn script_middleware_for_syncback(
    instance: &Instance,
    old: Option<InstanceWithMeta>,
    project: &Project,
) -> Option<Middleware> {
    if instance.class.as_str() == "Script"
        && get_property_filter(project, instance)
            .is_some_and(|ignored| ignored.contains(&"RunContext".into()))
    {
        if let Some(middleware) = old
            .filter(|old| old.class_name() == "Script")
            .and_then(|old| old.metadata().middleware)
            .filter(|middleware| file_names::script_kind_suffix(*middleware).is_some())
        {
            return Some(middleware);
        }
    }
    file_names::script_middleware_for_instance(instance)
}

pub fn get_best_middleware(snapshot: &SyncbackSnapshot) -> Middleware {
    // At some point, we're better off using an O(1) method for checking
    // equality for classes like this.
    static JSON_MODEL_CLASSES: OnceLock<HashSet<&str>> = OnceLock::new();
    let json_model_classes = JSON_MODEL_CLASSES.get_or_init(|| {
        [
            "Actor",
            "Sound",
            "SoundGroup",
            "Sky",
            "Atmosphere",
            "BloomEffect",
            "BlurEffect",
            "ColorCorrectionEffect",
            "DepthOfFieldEffect",
            "SunRaysEffect",
            "ParticleEmitter",
            "TextChannel",
            "TextChatCommand",
            // TODO: Implement a way to use inheritance for this
            "ChatWindowConfiguration",
            "ChatInputBarConfiguration",
            "BubbleChatConfiguration",
            "ChannelTabsConfiguration",
            "RemoteEvent",
            "UnreliableRemoteEvent",
            "RemoteFunction",
            "BindableEvent",
            "BindableFunction",
        ]
        .into()
    });

    let old_middleware = snapshot
        .old_inst()
        .and_then(|inst| inst.metadata().middleware);
    let inst = snapshot.new_inst();

    let mut middleware;

    if let Some(override_middleware) = snapshot.middleware {
        return override_middleware;
    } else if let Some(model) = snapshot.model_tree_middleware() {
        return model;
    } else if let Some(script) =
        script_middleware_for_syncback(inst, snapshot.old_inst(), snapshot.project()).filter(|_| {
            old_middleware.is_none_or(|old| file_names::script_kind_suffix(old).is_some())
        })
    {
        if !inst.children().is_empty() || old_middleware.is_some_and(|old| old.is_dir()) {
            return file_names::directory_variant(script);
        }
        return script;
    } else if old_middleware == Some(Middleware::Project) {
        return Middleware::Project;
    } else if inst.class == "Terrain"
        && !snapshot
            .old_inst()
            .is_some_and(|old| is_project_owned(old.metadata()))
    {
        return Middleware::Rbxm;
    } else if instances_as_directories(snapshot.project()) {
        return directory_middleware_for_instance(inst);
    } else if let Some(old_middleware) = old_middleware {
        return old_middleware;
    } else if json_model_classes.contains(inst.class.as_str()) {
        middleware = Middleware::JsonModel;
    } else {
        middleware = match inst.class.as_str() {
            "Folder" | "Configuration" | "Tool" => Middleware::Dir,
            "StringValue" => Middleware::Text,
            "Script" => Middleware::ServerScript,
            "LocalScript" => Middleware::ClientScript,
            "ModuleScript" => Middleware::ModuleScript,
            "LocalizationTable" => Middleware::Csv,
            // This isn't the ideal way to handle this but it works.
            name if name.ends_with("Value") => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    if !inst.children().is_empty() {
        middleware = file_names::directory_variant(middleware)
    }

    if middleware == Middleware::Rbxm {
        middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
            Ok(value) if value == "1" => Middleware::Rbxmx,
            Ok(value) if value == "2" => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    middleware
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SyncbackRules {
    /// A list of subtrees in a file that will be ignored by Syncback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ignore_trees: Vec<String>,
    /// A list of patterns to check against the path an Instance would serialize
    /// to. If a path matches one of these, the Instance won't be syncbacked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore_paths: Vec<String>,
    /// A map of classes to properties to ignore for that class when doing
    /// syncback.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    ignore_properties: IndexMap<Ustr, Vec<Ustr>>,
    /// Whether or not the `CurrentCamera` of `Workspace` is included in the
    /// syncback or not. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_current_camera: Option<bool>,
    /// Whether or not to sync properties that cannot be modified via scripts.
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_unscriptable: Option<bool>,
    /// Whether to skip serializing referent properties like `Model.PrimaryPart`
    /// during syncback. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_referents: Option<bool>,
    /// Whether the globs specified in `ignore_paths` should be modified to also
    /// match directories. Defaults to `true`.
    ///
    /// If this is `true`, it'll take ignore globs that end in `/**` and convert
    /// them to also handle the directory they're referring to. This is
    /// generally a better UX.
    #[serde(skip_serializing_if = "Option::is_none")]
    create_ignore_dir_paths: Option<bool>,
    /// Whether every non-script Instance should be written as a directory with
    /// an `init.meta.json` file instead of a model file. Siblings with
    /// duplicate or file system unsafe names get unique directory names, and
    /// their real names are stored in their metadata. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    instances_as_directories: Option<bool>,
    /// A list of rules that store whole subtrees in a single model file. Each
    /// has a `pattern` glob that is matched against the path of an Instance in
    /// the DataModel, and an optional `format` of `rbxm` (the default) or
    /// `rbxmx`. A matching Instance is written with all of its descendants to
    /// one file, even when `instancesAsDirectories` is set. The first rule
    /// that matches an Instance decides its format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) model_trees: Vec<ModelTreeRule>,
}

impl SyncbackRules {
    pub fn instances_as_directories(&self) -> bool {
        self.instances_as_directories.unwrap_or_default()
    }

    pub fn compile_globs(&self) -> anyhow::Result<Vec<IgnorableGlob>> {
        let mut globs = Vec::with_capacity(self.ignore_paths.len());
        let dir_ignore_paths = self.create_ignore_dir_paths.unwrap_or(true);

        for pattern in &self.ignore_paths {
            let glob = IgnorableGlob::new(pattern)
                .with_context(|| format!("the pattern '{pattern}' is not a valid glob"))?;
            globs.push(glob);

            if dir_ignore_paths {
                if let Some(dir_pattern) = pattern.strip_suffix("/**") {
                    if let Ok(glob) = IgnorableGlob::new(dir_pattern) {
                        globs.push(glob)
                    }
                }
            }
        }

        Ok(globs)
    }
}

pub(crate) fn is_valid_path(
    globs: &Option<Vec<IgnorableGlob>>,
    base_path: &Path,
    path: &Path,
) -> bool {
    let git_glob = GIT_IGNORE_GLOB.get_or_init(|| Glob::new(".git/**").unwrap());
    let test_path = match path.strip_prefix(base_path) {
        Ok(suffix) => suffix,
        Err(_) => path,
    };
    if git_glob.is_match(test_path) {
        return false;
    }
    if let Some(ref ignore_paths) = globs {
        // Gitignore-style "last match wins"
        let mut ignored = false;
        for glob in ignore_paths {
            if glob.is_match(test_path) {
                ignored = !glob.is_negation();
            }
        }
        if ignored {
            return false;
        }
    }
    true
}

/// Returns whether the project asks for every non-script Instance to be written
/// as a directory.
pub(crate) fn instances_as_directories(project: &Project) -> bool {
    project
        .syncback_rules
        .as_ref()
        .is_some_and(SyncbackRules::instances_as_directories)
}

/// Returns a set of properties that should not be written with syncback if
/// one exists. This list is read directly from the Project and takes
/// inheritance into effect.
///
/// It **does not** handle properties that should not serialize for other
/// reasons, such as being defaults or being marked as not serializing in the
/// ReflectionDatabase.
pub(crate) fn get_property_filter(project: &Project, new_inst: &Instance) -> Option<UstrSet> {
    let filter = &project.syncback_rules.as_ref()?.ignore_properties;
    let mut set = UstrSet::default();

    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = new_inst.class.as_str();

    loop {
        if let Some(list) = filter.get(&ustr(current_class_name)) {
            for name in list {
                set.insert(*name);
                set.insert(ustr(canonical_property_name(
                    &new_inst.class,
                    name.as_str(),
                )));
            }
        }

        let class = database.classes.get(current_class_name)?;
        if let Some(super_class) = class.superclass {
            current_class_name = super_class;
        } else {
            break;
        }
    }

    Some(set)
}

/// Produces a list of descendants in the WeakDom such that all children come
/// before their parents.
fn descendants(dom: &WeakDom, root_ref: Ref) -> Vec<Ref> {
    let mut queue = VecDeque::new();
    let mut ordered = Vec::new();
    queue.push_front(root_ref);

    while let Some(referent) = queue.pop_front() {
        let inst = dom
            .get_by_ref(referent)
            .expect("Invariant: WeakDom had a Ref that wasn't inside it");
        ordered.push(referent);
        for child in inst.children() {
            queue.push_back(*child)
        }
    }

    ordered
}

/// Removes the children of `new`'s root that are not also children of `old`'s
/// root.
///
/// This does not care about duplicates, and only filters based on names and
/// class names.
fn strip_unknown_root_children(new: &mut WeakDom, old: &RojoTree) {
    let old_root = old.root();
    let old_root_children: HashMap<&str, InstanceWithMeta> = old_root
        .children()
        .iter()
        .map(|referent| {
            let inst = old
                .get_instance(*referent)
                .expect("all children of a DOM's root should exist");
            (inst.name(), inst)
        })
        .collect();

    let root_children = new.root().children().to_vec();

    for child_ref in root_children {
        let child = new
            .get_by_ref(child_ref)
            .expect("all children of the root should exist in the DOM");
        if let Some(old) = old_root_children.get(child.name.as_str()) {
            if old.class_name() == child.class {
                continue;
            }
        }
        log::trace!("Pruning root child {} of class {}", child.name, child.class);
        new.destroy(child_ref);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::snapshot::{InstanceMetadata, InstanceSnapshot};
    use rbx_dom_weak::{types::Attributes, UstrMap};

    #[test]
    fn write_scope_validation_output_captures_lexical_write_parent_evidence() {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        vfs.set_watch_enabled(false);
        vfs.create_dir_all("/project/src").unwrap();
        let source = PathBuf::from("/project/src/Code.server.luau");
        vfs.write(&source, "return 1").unwrap();
        vfs.write(
            "/project/default.project.json",
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project =
            Project::load_exact(&vfs, Path::new("/project/default.project.json"), None).unwrap();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .name("Code")
                .class_name("Script")
                .metadata(
                    InstanceMetadata::new()
                        .instigating_source(source.clone())
                        .middleware(Middleware::RunContextServerScript),
                ),
        );
        let contexts = &rbx_reflection_database::get().unwrap().enums["RunContext"].items;
        let edited = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Script")
                .with_name("Code")
                .with_property("Source", "return 2")
                .with_property(
                    "RunContext",
                    rbx_dom_weak::types::Enum::from_u32(contexts["Client"]),
                ),
        );
        let (_, migration) =
            syncback_script_migration(&vfs, &tree, &project, tree.get_root_id(), edited).unwrap();
        let migration = migration.unwrap();
        let ownership = migration.ownership;
        for path in ownership
            .write_authority
            .as_ref()
            .unwrap()
            .paths
            .iter()
            .chain([&migration.source, &migration.destination])
        {
            let parent = path.parent().unwrap();
            assert_eq!(
                ownership.paths.get(parent),
                Some(&vfs.lock().normalize(parent).unwrap()),
                "Missing frozen lexical parent for {path:?}"
            );
        }
    }

    fn write_scope_validation_fixture() -> (Vfs, ScriptMigrationOwnership, Vec<PathBuf>) {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        vfs.set_watch_enabled(false);
        vfs.create_dir_all("/project/P").unwrap();
        vfs.create_dir_all("/project/Peer").unwrap();
        let writes = ["Code.server.luau", "Code.luau"]
            .map(|name| Path::new("/project/P").join(name))
            .to_vec();
        vfs.write(&writes[0], "source").unwrap();
        vfs.write("/project/Peer/Untouched.luau", "peer").unwrap();
        vfs.write("/project/main.project.json", "declaration")
            .unwrap();
        let mut ownership = ScriptMigrationOwnership::default();
        ownership
            .capture(&vfs, Path::new("/project/main.project.json"))
            .unwrap();
        ownership
            .capture(&vfs, Path::new("/project/optional.project.json"))
            .unwrap();
        ownership.absent.insert(PathBuf::from("/project/optional"));
        ownership
            .files
            .insert(PathBuf::from("/project/Peer/Untouched.luau"));
        for parent in [Path::new("/project/P"), Path::new("/project/Peer")] {
            ownership.identity(&vfs, parent).unwrap();
            let mut names = vfs
                .read_dir(parent)
                .unwrap()
                .map(|entry| PathBuf::from(entry.unwrap().path().file_name().unwrap()))
                .collect::<Vec<_>>();
            names.sort();
            ownership.directories.insert(parent.to_owned(), names);
        }
        ownership.write_authority = Some(ScriptMigrationWriteAuthority {
            source: writes[0].clone(),
            destination: writes[1].clone(),
            paths: writes
                .iter()
                .cloned()
                .chain([PathBuf::from("/project/Peer/Untouched.luau")])
                .collect(),
        });
        (vfs, ownership, writes)
    }

    #[test]
    fn write_scope_validation_keeps_declarations_absence_inventory_and_actual_write_scope() {
        for change in [
            "new_declaration",
            "deleted_declaration",
            "changed_declaration",
            "absence",
            "unexpected_entry",
            "authority_superset",
            "file_kind",
        ] {
            for complete in [false, true] {
                let (vfs, mut ownership, writes) = write_scope_validation_fixture();
                if change == "authority_superset" {
                    ownership
                        .files
                        .remove(Path::new("/project/Peer/Untouched.luau"));
                }
                if !complete {
                    ownership.paths.remove(Path::new("/project/P"));
                }
                ownership.validate(&mut vfs.lock(), &writes).unwrap();
                match change {
                    "new_declaration" => {
                        vfs.write("/project/optional.project.json", "new").unwrap()
                    }
                    "deleted_declaration" => vfs.remove_file("/project/main.project.json").unwrap(),
                    "changed_declaration" => {
                        vfs.write("/project/main.project.json", "changed").unwrap()
                    }
                    "absence" => vfs.create_dir_all("/project/optional").unwrap(),
                    "unexpected_entry" => vfs.write("/project/P/Foreign.luau", "foreign").unwrap(),
                    "authority_superset" => {
                        vfs.remove_file("/project/Peer/Untouched.luau").unwrap()
                    }
                    "file_kind" => {
                        vfs.remove_file("/project/Peer/Untouched.luau").unwrap();
                        vfs.create_dir_all("/project/Peer/Untouched.luau").unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    ownership.validate(&mut vfs.lock(), &writes).is_err(),
                    "Accepted {change}, complete={complete}"
                );
            }
        }
        let (vfs, ownership, writes) = write_scope_validation_fixture();
        // A staged source is exempt only when it is an actual write, not merely allowed.
        vfs.remove_file(&writes[0]).unwrap();
        assert!(ownership.validate(&mut vfs.lock(), &[]).is_err());
        assert!(ownership.validate(&mut vfs.lock(), &writes[1..]).is_err());
        ownership.validate(&mut vfs.lock(), &writes).unwrap();
        let restored: ScriptMigrationOwnership =
            serde_json::from_slice(&serde_json::to_vec(&ownership).unwrap()).unwrap();
        restored.validate(&mut vfs.lock(), &writes).unwrap();
    }

    #[test]
    fn write_scope_validation_one_missing_parent_keeps_the_entire_exhaustive_loop() {
        use memofs::{test_support::RecordingBackend, InMemoryFs};
        let (vfs, mut ownership, mut writes) = write_scope_validation_fixture();
        writes.push(PathBuf::from("/project/Peer/Untouched.luau"));
        ownership.paths.remove(Path::new("/project/Peer"));
        let mut memory = InMemoryFs::new();
        memory
            .load_snapshot(
                "/project",
                vfs.snapshot_tree(Path::new("/project")).unwrap(),
            )
            .unwrap();
        let (backend, handle) = RecordingBackend::new(memory);
        let recorded = Vfs::new(backend);
        recorded.set_watch_enabled(false);
        handle.start();
        ownership.validate(&mut recorded.lock(), &writes).unwrap();
        let counts = handle.stop();
        for (parent, expected) in [("/project/P", 6), ("/project/Peer", 3)] {
            assert_eq!(
                counts
                    .iter()
                    .filter(|(call, _)| call.operation == "canonicalize"
                        && call.paths == [Path::new(parent).as_os_str()])
                    .map(|(_, count)| count)
                    .sum::<u64>(),
                expected
            );
        }
        recorded
            .write("/project/Peer/Foreign.luau", "foreign")
            .unwrap();
        assert!(ownership.validate(&mut recorded.lock(), &writes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn write_scope_validation_native_aliases_keep_original_write_order_and_persistent_identity() {
        use memofs::{test_support::RecordingBackend, StdBackend};
        use std::sync::{Arc, Mutex};
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let parent = root.join("P");
        std::fs::create_dir(&parent).unwrap();
        let a = root.join("A");
        let b = root.join("B");
        let directory = root.join("D");
        for alias in [&a, &b, &directory] {
            std::os::unix::fs::symlink(&parent, alias).unwrap();
        }
        let plain = Vfs::new(StdBackend::new().unwrap());
        plain.set_watch_enabled(false);
        let mut ownership = ScriptMigrationOwnership::default();
        for alias in [&a, &b] {
            ownership.identity(&plain, alias).unwrap();
        }
        ownership.directories.insert(directory.clone(), vec![]);
        let writes = vec![
            b.join("Code.luau"),
            a.join("Code.meta.json"),
            b.join("Code.server.luau"),
        ];
        let state = Arc::new(Mutex::new((false, Vec::new())));
        let hook_state = state.clone();
        let hook_directory = directory.clone();
        let (backend, _) = RecordingBackend::new(StdBackend::new().unwrap());
        let backend = backend.with_after_observation(move |operation, path| {
            let mut state = hook_state.lock().unwrap();
            if path == hook_directory {
                state.0 = operation == "canonicalize";
            } else if state.0 && operation == "canonicalize" {
                state.1.push(path.to_owned());
            }
        });
        let vfs = Vfs::new(backend);
        vfs.set_watch_enabled(false);
        ownership.validate(&mut vfs.lock(), &writes).unwrap();
        assert_eq!(
            state.lock().unwrap().1,
            vec![b.clone(), a.clone(), b.clone()]
        );
        // Durable evidence is lexical-parent evidence, not the parent of a
        // canonical file spelling. A persistent retarget must fail after restart.
        let mut ownership: ScriptMigrationOwnership =
            serde_json::from_slice(&serde_json::to_vec(&ownership).unwrap()).unwrap();
        let hidden = root.join("H");
        std::fs::rename(&parent, &hidden).unwrap();
        std::os::unix::fs::symlink(&hidden, &parent).unwrap();
        for complete in [true, false] {
            if !complete {
                ownership.paths.remove(&a);
            }
            assert!(ownership.validate(&mut vfs.lock(), &writes).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_scope_validation_native_transient_parent_observations_and_after_guard() {
        use memofs::{test_support::RecordingBackend, StdBackend};
        use std::sync::{Arc, Mutex};
        // D is a lexical inventory alias, so its canonicalization unambiguously
        // identifies the directory predicate, not an earlier path-evidence check.
        for change in ["after_directory", "between_writes", "after_inventory"] {
            for complete in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let root = dunce::canonicalize(temp.path()).unwrap();
                let parent = root.join("P");
                let hidden = root.join("H");
                let directory = root.join("D");
                std::fs::create_dir(&parent).unwrap();
                std::os::unix::fs::symlink(&parent, &directory).unwrap();
                let source = parent.join("Code.server.luau");
                let destination = parent.join("Code.luau");
                let plain = Vfs::new(StdBackend::new().unwrap());
                plain.set_watch_enabled(false);
                let mut ownership = ScriptMigrationOwnership::default();
                ownership.identity(&plain, &parent).unwrap();
                // The source has already been staged. Its frozen inventory still
                // includes it, making inappropriate write exemptions observable.
                ownership
                    .directories
                    .insert(directory.clone(), vec![PathBuf::from("Code.server.luau")]);
                let writes = vec![destination, source];
                if !complete {
                    ownership.paths.remove(&parent);
                }
                let state = Arc::new(Mutex::new((false, 0usize, false, false)));
                let hook_state = state.clone();
                let hook_parent = parent.clone();
                let hook_hidden = hidden.clone();
                let hook_directory = directory.clone();
                let (backend, _) = RecordingBackend::new(StdBackend::new().unwrap());
                let backend = backend.with_after_observation(move |operation, path| {
                    let mut state = hook_state.lock().unwrap();
                    if operation == "canonicalize" && path == hook_directory {
                        state.0 = true;
                    } else if operation == "canonicalize" && path == hook_parent && state.0 {
                        state.1 += 1;
                    }
                    let trigger = !state.2
                        && match change {
                            "after_directory" => {
                                operation == "canonicalize" && path == hook_directory
                            }
                            "between_writes" => {
                                operation == "canonicalize"
                                    && path == hook_parent
                                    && state.0
                                    && state.1 == 1
                            }
                            "after_inventory" => operation == "read_dir" && path == hook_directory,
                            _ => unreachable!(),
                        };
                    if trigger {
                        std::fs::rename(&hook_parent, &hook_hidden).unwrap();
                        std::os::unix::fs::symlink(&hook_hidden, &hook_parent).unwrap();
                        state.2 = true;
                    }
                    if operation == "read_dir"
                        && path == hook_directory
                        && state.2
                        && matches!(change, "after_directory" | "between_writes")
                    {
                        std::fs::remove_file(&hook_parent).unwrap();
                        std::fs::rename(&hook_hidden, &hook_parent).unwrap();
                        state.3 = true;
                    }
                });
                let vfs = Vfs::new(backend);
                vfs.set_watch_enabled(false);
                let result = ownership.validate(&mut vfs.lock(), &writes);
                let state = *state.lock().unwrap();
                assert!(state.2, "Race did not fire for {change}");
                if hidden.exists() {
                    std::fs::remove_file(&parent).unwrap();
                    std::fs::rename(&hidden, &parent).unwrap();
                }
                // Incomplete journals keep the historical loop, including its lack
                // of an after guard. They still reject the reviewed transient race.
                let refuses = complete || matches!(change, "after_directory" | "between_writes");
                assert_eq!(
                    result.is_err(),
                    refuses,
                    "Race {change}, complete={complete}, {result:?}"
                );
                if change == "between_writes" {
                    assert_eq!(state.1, 2);
                }
                if !complete && change == "after_directory" {
                    assert!(state.3);
                }
                assert!(!parent.join("Code.server.luau").exists());
                assert!(!parent.join("Code.luau").exists());
            }
        }
    }

    #[test]
    fn canonical_dependency_allows_resolved_unicode_siblings_in_memory() {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        vfs.set_watch_enabled(false);
        vfs.create_dir_all("/project/Other/Piña colada").unwrap();
        vfs.write("/project/Other/Σ.luau", "return 1").unwrap();
        let mut fs = vfs.lock();
        for tail in [
            "default.project.json",
            "default.project.jsonc",
            "New/Child.luau",
        ] {
            let path = Path::new("/project/Other").join(tail);
            assert_eq!(canonical_dependency_path(&mut fs, &path), Some(path));
        }
        assert_eq!(
            canonical_dependency_path(&mut fs, Path::new("/project/Other/Absentü.luau")),
            None
        );
        assert_eq!(
            canonical_dependency_path(&mut fs, Path::new("Other/New")),
            None
        );
    }

    #[test]
    fn canonical_dependency_allows_resolved_unicode_siblings_on_disk() {
        let temporary = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temporary.path()).unwrap();
        std::fs::create_dir(root.join("Piña colada")).unwrap();
        std::fs::write(root.join("Σ.luau"), "return 1").unwrap();
        let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
        vfs.set_watch_enabled(false);
        let mut fs = vfs.lock();
        for tail in [
            "default.project.json",
            "default.project.jsonc",
            "New/Child.luau",
        ] {
            let path = root.join(tail);
            assert_eq!(canonical_dependency_path(&mut fs, &path), Some(path));
        }
    }

    #[test]
    fn canonical_dependency_rejects_missing_ascii_case_alias() {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        vfs.set_watch_enabled(false);
        vfs.create_dir_all("/project/Other").unwrap();
        vfs.write("/project/Other/DEFAULT.project.json", "{}")
            .unwrap();
        assert_eq!(
            canonical_dependency_path(
                &mut vfs.lock(),
                Path::new("/project/Other/default.project.json"),
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_dependency_rejects_unresolved_unicode_siblings() {
        for target in ["missing", "Piña colada"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(temporary.path()).unwrap();
            let candidate = root.join("Piña colada");
            std::os::unix::fs::symlink(target, &candidate).unwrap();
            let vfs = Vfs::new(memofs::StdBackend::new().unwrap());
            vfs.set_watch_enabled(false);
            let mut fs = vfs.lock();
            let error = fs.normalize(&candidate).unwrap_err();
            if target == "missing" {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            } else {
                assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
            }
            assert_eq!(
                canonical_dependency_path(&mut fs, &root.join("default.project.json")),
                None
            );
        }
    }

    #[test]
    fn migration_ownership_with_unicode_siblings_revalidates_commit_evidence() {
        for change in ["absence", "identity", "directory", "bytes"] {
            let vfs = Vfs::new(memofs::InMemoryFs::new());
            vfs.set_watch_enabled(false);
            vfs.create_dir_all("/project/Fixture").unwrap();
            vfs.create_dir_all("/project/Other/Piña colada").unwrap();
            vfs.write("/project/Fixture/Code.server.luau", "print(0)")
                .unwrap();
            vfs.write("/project/default.project.json", r#"{"name":"Test","tree":{"$className":"Folder","Fixture":{"$path":"Fixture"},"Other":{"$path":"Other"},"Absent":{"$path":{"optional":"extras"}}}}"#).unwrap();
            let project =
                Project::load_exact(&vfs, Path::new("/project/default.project.json"), None)
                    .unwrap();
            let writes = [
                PathBuf::from("/project/Fixture/Code.server.luau"),
                PathBuf::from("/project/Fixture/Code.client.luau"),
            ];
            let ownership = script_migration_projects(
                &vfs,
                &Default::default(),
                &project,
                &[&writes[0], &writes[1]],
            )
            .unwrap();
            ownership.validate(&mut vfs.lock(), &writes).unwrap();
            match change {
                "absence" => vfs.create_dir_all("/project/extras").unwrap(),
                "identity" => vfs
                    .write("/project/Other/DEFAULT.project.json", "{}")
                    .unwrap(),
                // Only folders on the way to the script are listed, so unrelated
                // folders such as Other can change without invalidating the rename.
                "directory" => vfs.create_dir_all("/project/Fixture/New").unwrap(),
                "bytes" => vfs
                    .write("/project/default.project.json", "foreign declaration")
                    .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                ownership.validate(&mut vfs.lock(), &writes).is_err(),
                "Accepted {change}"
            );
            assert_eq!(
                vfs.read("/project/Fixture/Code.server.luau")
                    .unwrap()
                    .as_slice(),
                b"print(0)"
            );
            assert!(!vfs.exists("/project/Fixture/Code.client.luau").unwrap());
        }
    }

    #[test]
    fn migration_ownership_resolves_installed_package_reservations() {
        let vfs = Vfs::new(memofs::InMemoryFs::new());
        vfs.set_watch_enabled(false);
        for path in [
            "/project/Local/Library",
            "/project/Packages/_Index/Library/library",
        ] {
            vfs.create_dir_all(path).unwrap();
        }
        let manifest = "[package]\nname = \"scope/app\"\nversion = \"1.0.0\"\nrealm = \"shared\"\n[dependencies]\nLibrary = \"^Local/Library\"\n";
        let local = "[package]\nname = \"scope/library\"\nrealm = \"shared\"\n";
        vfs.write("/project/wally.toml", manifest).unwrap();
        vfs.write("/project/Local/Library/wally.toml", local)
            .unwrap();
        vfs.write("/project/wally.install.json", serde_json::to_vec(&serde_json::json!({
            "version":1,
            "manifest_hash":crate::package_sources::sha256(manifest.as_bytes()),
            "local_manifests":{"Local/Library/wally.toml":crate::package_sources::sha256(local.as_bytes())},
            "package_roots":[{"name":"scope/library", "kind":"local", "realm":"shared", "install_path":"Packages/_Index/Library/library", "source_path":"Local/Library"}],
            "generated_paths":[]
        })).unwrap()).unwrap();
        vfs.write("/project/Local/Library/Code.legacy.luau", "print(0)")
            .unwrap();
        let mut context = crate::snapshot::InstanceContext::default();
        context.package_sources =
            crate::package_sources::PackageSources::load(&vfs, Path::new("/project"))
                .unwrap()
                .map(std::sync::Arc::new);
        assert!(context.package_sources.as_ref().unwrap().problem.is_none());
        for name in ["Code", "Spare"] {
            vfs.write("/project/default.project.json", format!(r#"{{"name":"Test","tree":{{"$className":"Folder","$path":"Local/Library","Reserved":{{"$path":{{"optional":"Packages/_Index/Library/library/{name}.client.luau"}}}}}}}}"#)).unwrap();
            let project =
                Project::load_exact(&vfs, Path::new("/project/default.project.json"), None)
                    .unwrap();
            let result = script_migration_projects(
                &vfs,
                &context,
                &project,
                &[
                    Path::new("/project/Local/Library/Code.legacy.luau"),
                    Path::new("/project/Local/Library/Code.client.luau"),
                ],
            );
            if name == "Code" {
                assert!(result.unwrap_err().to_string().contains("project mapping"));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn migration_ownership_survives_restart_and_guards_declaration_graph() {
        use super::relocation_transaction::{
            execute_script_replacements_guarded, recover, Recovery, Replacement,
        };
        use memofs::{InMemoryFs, VfsSnapshot};

        for change in [
            "unchanged",
            "custom",
            "probe",
            "directory",
            "absent",
            "file_kind",
        ] {
            let mut backend = InMemoryFs::new();
            backend.load_snapshot("/project", VfsSnapshot::dir([
                ("default.project.json", VfsSnapshot::file(r#"{"name":"Test","syncRules":[{"pattern":"*.ref","use":"project"}],"tree":{"$className":"Folder","$path":"src","Reserved":{"$path":{"optional":"reserved.ref"}},"Absent":{"$path":{"optional":"extras"}}}}"#)),
                ("reserved.ref", VfsSnapshot::file(r#"{"name":"Reserved","tree":{"$path":{"optional":"src/Spare.luau"}}}"#)),
                ("src", VfsSnapshot::dir([
                    ("Code.legacy.luau", VfsSnapshot::file("old source")),
                    ("Other.luau", VfsSnapshot::file("unrelated payload")),
                ])),
            ])).unwrap();
            let vfs = Vfs::new(backend);
            vfs.set_watch_enabled(false);
            let project =
                Project::load_exact(&vfs, Path::new("/project/default.project.json"), None)
                    .unwrap();
            let source = PathBuf::from("/project/src/Code.legacy.luau");
            let destination = PathBuf::from("/project/src/Code.client.luau");
            let ownership = script_migration_projects(
                &vfs,
                &Default::default(),
                &project,
                &[&source, &destination],
            )
            .unwrap();
            let mut expected = ownership.declarations.clone();
            expected.insert(source.clone(), Some(b"old source".to_vec()));
            expected.insert(destination.clone(), None);
            let entries = vec![
                Replacement {
                    path: source,
                    expected: Some(VfsSnapshot::file("old source")),
                    desired: None,
                },
                Replacement {
                    path: destination,
                    expected: None,
                    desired: Some(VfsSnapshot::file("new source")),
                },
            ];
            let journal = Path::new("/project/.rojo/relocations/script");
            vfs.create_dir_all(journal.parent().unwrap()).unwrap();
            execute_script_replacements_guarded(
                &vfs,
                journal,
                &project,
                entries,
                &expected,
                Some(&ownership),
            )
            .unwrap();
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot(
                    "/project",
                    vfs.snapshot_tree(Path::new("/project")).unwrap(),
                )
                .unwrap();
            let restarted = Vfs::new(backend);
            restarted.set_watch_enabled(false);
            match change {
                "custom" => restarted
                    .write("/project/reserved.ref", "foreign declaration")
                    .unwrap(),
                "probe" => restarted
                    .write("/project/src/default.project.json", "foreign declaration")
                    .unwrap(),
                "directory" => restarted.create_dir_all("/project/src/New").unwrap(),
                "absent" => restarted.create_dir_all("/project/extras").unwrap(),
                "file_kind" => {
                    restarted.remove_file("/project/src/Other.luau").unwrap();
                    restarted.create_dir_all("/project/src/Other.luau").unwrap();
                }
                _ => {}
            }
            let before = restarted.snapshot_tree(Path::new("/project")).unwrap();
            // The journal is committed, so later declaration changes cannot block its
            // cleanup, and acknowledging it must never touch the project.
            assert_eq!(
                recover(&restarted, journal).unwrap(),
                Recovery::Committed,
                "Refused committed journal after {change}"
            );
            assert_eq!(
                restarted.snapshot_tree(Path::new("/project")).unwrap(),
                before
            );
        }
    }

    #[test]
    fn migration_ownership_work_is_bounded_by_the_target_ancestry() {
        use memofs::{InMemoryFs, VfsSnapshot};

        // A rename must not scale with the rest of the project. Unrelated subtrees are
        // probed for project files but never walked, so their size changes nothing.
        let capture = |unrelated: usize| {
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot(
                    "/project",
                    VfsSnapshot::dir([
                        (
                            "default.project.json",
                            VfsSnapshot::file(
                                r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#,
                            ),
                        ),
                        (
                            "src",
                            VfsSnapshot::dir([
                                (
                                    "Target",
                                    VfsSnapshot::dir([(
                                        "Code.legacy.luau",
                                        VfsSnapshot::file("source"),
                                    )]),
                                ),
                                (
                                    "Unrelated",
                                    VfsSnapshot::dir((0..unrelated).map(|index| {
                                        (
                                            format!("Folder{index}"),
                                            VfsSnapshot::dir([(
                                                "Leaf",
                                                VfsSnapshot::dir([(
                                                    "Module.luau",
                                                    VfsSnapshot::file("return 1"),
                                                )]),
                                            )]),
                                        )
                                    })),
                                ),
                            ]),
                        ),
                    ]),
                )
                .unwrap();
            let vfs = Vfs::new(backend);
            vfs.set_watch_enabled(false);
            let project =
                Project::load_exact(&vfs, Path::new("/project/default.project.json"), None)
                    .unwrap();
            let source = PathBuf::from("/project/src/Target/Code.legacy.luau");
            let destination = PathBuf::from("/project/src/Target/Code.server.luau");
            script_migration_projects(
                &vfs,
                &Default::default(),
                &project,
                &[&source, &destination],
            )
            .unwrap()
        };
        let small = capture(2);
        let large = capture(60);
        assert_eq!(small.paths.len(), large.paths.len());
        assert_eq!(small.directories.len(), large.directories.len());
        assert_eq!(small.files.len(), large.files.len());
        assert_eq!(small.declarations.len(), large.declarations.len());
        assert!(!large
            .directories
            .contains_key(Path::new("/project/src/Unrelated")));
        assert!(large
            .directories
            .contains_key(Path::new("/project/src/Target")));
    }

    #[test]
    fn sound_alias_project_syncback_cleans_keys_even_without_value_changes() {
        use rbx_dom_weak::InstanceBuilder;
        for with_path in [false, true] {
            for properties in [
                r#""EmitterSize":5"#,
                r#""EmitterSize":3,"RollOffMinDistance":5"#,
                r#""RollOffMinDistance":5,"EmitterSize":3"#,
            ] {
                for value in [10.0, 7.0, 5.0] {
                    let temp = tempfile::tempdir().unwrap();
                    let root = temp.path();
                    let path = root.join("default.project.json");
                    let vfs = Vfs::new_default().unwrap();
                    vfs.set_watch_enabled(false);
                    let source = if with_path {
                        fs_err::create_dir(root.join("Sound")).unwrap();
                        r#""$path":"Sound","#
                    } else {
                        ""
                    };
                    vfs.write(&path, format!(
                        r#"{{"name":"Test","tree":{{"$className":"Folder","Sound":{{"$className":"Sound",{source}"$properties":{{{properties}}}}}}}}}"#
                    )).unwrap();
                    let project = Project::load_exact(&vfs, &path, Some("Test")).unwrap();
                    let old = crate::snapshot_middleware::snapshot_from_vfs(
                        &Default::default(),
                        &vfs,
                        &path,
                    )
                    .unwrap()
                    .unwrap();
                    let tree = RojoTree::new(old);
                    let input = WeakDom::new(
                        InstanceBuilder::new("Folder").with_name("Test").with_child(
                            InstanceBuilder::new("Sound")
                                .with_name("Sound")
                                .with_property("RollOffMinDistance", Variant::Float32(value)),
                        ),
                    );
                    // Exercise the project middleware's dirty contract directly. The
                    // outer loop may skip an unchanged tree before any merge occurs.
                    let attribute_ids = HashSet::new();
                    let snapshot = SyncbackSnapshot {
                        data: SyncbackData {
                            vfs: &vfs,
                            old_tree: &tree,
                            new_tree: &input,
                            project: &project,
                            scoped: false,
                            context: None,
                            script_containers: None,
                            model_trees: None,
                            attribute_ids: &attribute_ids,
                        },
                        old: Some(tree.get_root_id()),
                        new: input.root_ref(),
                        path: path.clone(),
                        middleware: Some(Middleware::Project),
                    };
                    let saved = Middleware::Project.syncback(&snapshot).unwrap().fs_snapshot;
                    assert!(
                        saved.added_paths().contains(&path.as_path()),
                        "path {with_path}, {properties}, {value}"
                    );
                    saved.write_to_vfs(root, &vfs).unwrap();
                    let text = vfs.read_to_string(&path).unwrap();
                    assert!(!text.contains("EmitterSize"), "{text}");
                    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let written = &json["tree"]["Sound"]["$properties"];
                    if value == 10.0 {
                        assert!(written.get("RollOffMinDistance").is_none(), "{text}");
                    } else {
                        assert_eq!(written["RollOffMinDistance"], value, "{text}");
                    }
                    let reloaded = crate::snapshot_middleware::snapshot_from_vfs(
                        &Default::default(),
                        &vfs,
                        &path,
                    )
                    .unwrap()
                    .unwrap();
                    assert!(!reloaded.children[0]
                        .properties
                        .contains_key(&ustr("EmitterSize")));
                    assert_eq!(
                        reloaded.children[0]
                            .properties
                            .get(&ustr("RollOffMinDistance"))
                            .cloned()
                            .unwrap_or(Variant::Float32(10.0)),
                        Variant::Float32(value)
                    );
                }
            }
        }
    }

    #[test]
    fn model_backed_project_overrides_do_not_restore_stale_sound_aliases() {
        use rbx_dom_weak::InstanceBuilder;
        for storage in ["model.json", "project.json"] {
            for value in [7.0, 10.0] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                let path = root.join("default.project.json");
                let source = format!("Sound.{storage}");
                let vfs = Vfs::new_default().unwrap();
                vfs.set_watch_enabled(false);
                vfs.write(
                    root.join(&source),
                    if storage == "model.json" {
                        r#"{"className":"Sound","properties":{"RollOffMinDistance":2}}"#
                    } else {
                        r#"{"name":"Sound","tree":{"$className":"Sound","$properties":{"RollOffMinDistance":2}}}"#
                    },
                ).unwrap();
                vfs.write(&path, format!(
                    r#"{{"name":"Test","tree":{{"$className":"Folder","Sound":{{"$path":"{source}","$properties":{{"EmitterSize":5}}}}}}}}"#
                )).unwrap();
                let project = Project::load_exact(&vfs, &path, Some("Test")).unwrap();
                let old =
                    crate::snapshot_middleware::snapshot_from_vfs(&Default::default(), &vfs, &path)
                        .unwrap()
                        .unwrap();
                let mut tree = RojoTree::new(old);
                let input = WeakDom::new(
                    InstanceBuilder::new("Folder").with_name("Test").with_child(
                        InstanceBuilder::new("Sound")
                            .with_name("Sound")
                            .with_property("RollOffMinDistance", Variant::Float32(value))
                            .with_property("Volume", Variant::Float32(0.25)),
                    ),
                );
                let saved = syncback_loop(&vfs, &mut tree, input, &project).unwrap();
                saved.write_to_vfs(root, &vfs).unwrap();
                let contents = vfs.read_to_string(&path).unwrap();
                assert!(
                    !contents.contains("EmitterSize"),
                    "{storage}, {value}, {contents}"
                );
                let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
                let overrides = &json["tree"]["Sound"]["$properties"];
                assert!(overrides.get("Volume").is_none(), "{contents}");
                if value == 10.0 {
                    assert!(overrides.get("RollOffMinDistance").is_none(), "{contents}");
                } else {
                    assert_eq!(overrides["RollOffMinDistance"], value, "{contents}");
                }
                let reloaded =
                    crate::snapshot_middleware::snapshot_from_vfs(&Default::default(), &vfs, &path)
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    reloaded.children[0]
                        .properties
                        .get(&ustr("RollOffMinDistance"))
                        .cloned()
                        .unwrap_or(Variant::Float32(10.0)),
                    Variant::Float32(value),
                    "{storage}, {value}, {contents}",
                );
                assert_eq!(
                    reloaded.children[0].properties.get(&ustr("Volume")),
                    Some(&Variant::Float32(0.25)),
                    "{storage}, {value}",
                );
            }
        }
    }

    #[test]
    fn sound_alias_filter_retains_configured_and_concrete_canonical_names() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let path = temp.path().join("default.project.json");
        vfs.write(&path, r#"{"tree":{"$className":"Folder"},"syncbackRules":{"ignoreProperties":{"Instance":["EmitterSize","UnknownProperty"],"Sound":["Volume"]}}}"#).unwrap();
        let project = Project::load_exact(&vfs, &path, Some("Test")).unwrap();
        let dom = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Sound"));
        let filter = get_property_filter(&project, dom.root()).unwrap();
        for name in [
            "EmitterSize",
            "RollOffMinDistance",
            "UnknownProperty",
            "Volume",
        ] {
            assert!(filter.contains(&ustr(name)), "Missing {name}");
        }
    }

    #[test]
    fn whole_tree_package_conflicts_return_errors_without_saving_any_edit() {
        for changed in ["Target", "Sibling"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            fs_err::create_dir(root.join("Source")).unwrap();
            let project_path = root.join("default.project.json");
            vfs.write(
                &project_path,
                r#"{"name":"Test","tree":{"$path":"Source"}}"#,
            )
            .unwrap();
            let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
            let baseline = "return 'baseline'\n";
            let filesystem = "return 'filesystem'\n";
            let mut children = Vec::new();
            for name in ["Target", "Sibling"] {
                let path = root.join(format!("Source/{name}.luau"));
                vfs.write(&path, baseline).unwrap();
                let mut metadata = InstanceMetadata::new();
                metadata
                    .package_file_contents
                    .insert(path, Some(baseline.as_bytes().to_vec()));
                children.push(
                    InstanceSnapshot::new()
                        .name(name)
                        .class_name("ModuleScript")
                        .properties(UstrMap::from_iter([(
                            ustr("Source"),
                            Variant::String(baseline.into()),
                        )]))
                        .metadata(metadata),
                );
            }
            let mut tree = RojoTree::new(
                InstanceSnapshot::new()
                    .name("Test")
                    .class_name("Folder")
                    .children(children),
            );
            let target = tree.root().children()[0];
            vfs.write(root.join(format!("Source/{changed}.luau")), filesystem)
                .unwrap();
            assert_eq!(
                package_source_conflict(&vfs, &tree, target).unwrap(),
                changed == "Target"
            );
            let edited = WeakDom::new(
                rbx_dom_weak::InstanceBuilder::new("Folder")
                    .with_name("Test")
                    .with_child(
                        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
                            .with_name("Target")
                            .with_property("Source", "return 'studio'\n"),
                    )
                    .with_child(
                        rbx_dom_weak::InstanceBuilder::new("ModuleScript")
                            .with_name("Sibling")
                            .with_property("Source", baseline),
                    ),
            );

            let error = syncback_loop(&vfs, &mut tree, edited, &project)
                .err()
                .expect("A rejected batch must not be acknowledged as an empty success");

            assert!(error
                .to_string()
                .contains("Package source changed concurrently"));
            for name in ["Target", "Sibling"] {
                assert_eq!(
                    fs_err::read_to_string(root.join(format!("Source/{name}.luau"))).unwrap(),
                    if name == changed {
                        filesystem
                    } else {
                        baseline
                    }
                );
            }
            for instance in tree.inner().descendants().skip(1) {
                assert_eq!(
                    instance.properties.get(&ustr("Source")),
                    Some(&Variant::String(baseline.into()))
                );
            }
            assert!(!root.join(".rojo").exists());
        }
    }

    #[test]
    fn scoped_script_writes_reject_suffix_changes_without_touching_disk() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project_path = temp.path().join("default.project.json");
        vfs.write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        for (name, class, middleware) in [
            (
                "Server.server.luau",
                "Script",
                Middleware::RunContextServerScript,
            ),
            (
                "Local.client.luau",
                "LocalScript",
                Middleware::LegacyClientScript,
            ),
        ] {
            let path = temp.path().join(name);
            vfs.write(&path, "return 1").unwrap();
            let tree = RojoTree::new(
                InstanceSnapshot::new()
                    .name("Code")
                    .class_name(class)
                    .metadata(
                        InstanceMetadata::new()
                            .instigating_source(path.clone())
                            .middleware(middleware),
                    ),
            );
            let mut builder = rbx_dom_weak::InstanceBuilder::new(class)
                .with_name("Code")
                .with_property("Source", "return 2");
            if class == "Script" {
                let contexts = &rbx_reflection_database::get().unwrap().enums["RunContext"].items;
                builder = builder.with_property(
                    "RunContext",
                    rbx_dom_weak::types::Enum::from_u32(contexts["Client"]),
                );
            }
            let error = syncback_instance(
                &vfs,
                &tree,
                &project,
                tree.get_root_id(),
                WeakDom::new(builder),
            )
            .err()
            .expect("A suffix-changing scoped write must be rejected");
            assert!(error.to_string().contains("full syncback"), "{error}");
            assert_eq!(fs_err::read_to_string(&path).unwrap(), "return 1");
            assert!(!temp.path().join("Code.meta.json").exists());
        }
    }

    #[test]
    fn scoped_instance_write_preserves_properties_and_orders_class_name_first() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project_path = temp.path().join("default.project.json");
        vfs.write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let instance_path = temp.path().join("Wire");
        let root = InstanceSnapshot::new()
            .name("Wire")
            .class_name("Part")
            .properties(UstrMap::from_iter([(
                ustr("Transparency"),
                Variant::Float32(0.25),
            )]))
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(instance_path.clone())
                    .middleware(Middleware::Dir),
            );
        let tree = RojoTree::new(root);
        let new_tree = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Part")
                .with_name("Wire")
                .with_properties([(ustr("Transparency"), Variant::Float32(0.75))]),
        );
        let snapshot =
            syncback_instance(&vfs, &tree, &project, tree.get_root_id(), new_tree).unwrap();
        snapshot.write_to_vfs(temp.path(), &vfs).unwrap();
        let contents = std::fs::read_to_string(instance_path.join("init.meta.json")).unwrap();
        assert!(contents.starts_with("{\n    \"className\": \"Part\""));
        assert!(contents.contains("\"Transparency\": 0.75"));
    }

    #[test]
    fn scoped_instance_write_keeps_existing_key_order() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project_path = temp.path().join("default.project.json");
        vfs.write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let instance_path = temp.path().join("Zone");
        std::fs::create_dir(&instance_path).unwrap();
        std::fs::write(
            instance_path.join("init.meta.json"),
            r#"{
                "className": "Part",
                "properties": {
                    "Locked": true,
                    "Anchored": true,
                    "Size": [1.100000023841858, 2.0, 3.0],
                    "Transparency": 0.25
                },
                "attributes": { "Zeta": 1, "Alpha": 2 }
            }"#,
        )
        .unwrap();

        let root = InstanceSnapshot::new()
            .name("Zone")
            .class_name("Part")
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(instance_path.clone())
                    .middleware(Middleware::Dir),
            );
        let tree = RojoTree::new(root);
        let attributes: Attributes = [
            ("Alpha".to_owned(), Variant::Float64(2.0)),
            ("Beta".to_owned(), Variant::Float64(3.0)),
            ("Zeta".to_owned(), Variant::Float64(1.0)),
        ]
        .into_iter()
        .collect();
        let new_tree = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Part")
                .with_name("Zone")
                .with_properties([
                    (ustr("CastShadow"), Variant::Bool(false)),
                    (ustr("Transparency"), Variant::Float32(0.6)),
                    (ustr("Anchored"), Variant::Bool(true)),
                    (ustr("Locked"), Variant::Bool(true)),
                    (
                        ustr("Size"),
                        Variant::Vector3(rbx_dom_weak::types::Vector3::new(1.1, 2.0, 3.0)),
                    ),
                    (ustr("Attributes"), Variant::Attributes(attributes)),
                ]),
        );
        let snapshot =
            syncback_instance(&vfs, &tree, &project, tree.get_root_id(), new_tree).unwrap();
        snapshot.write_to_vfs(temp.path(), &vfs).unwrap();
        let contents = std::fs::read_to_string(instance_path.join("init.meta.json")).unwrap();

        let position = |key: &str| {
            contents
                .find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("{key} missing from {contents}"))
        };
        assert!(position("Locked") < position("Anchored"), "{contents}");
        assert!(
            position("Anchored") < position("Transparency"),
            "{contents}"
        );
        assert!(
            position("Transparency") < position("CastShadow"),
            "{contents}"
        );
        assert!(position("Zeta") < position("Alpha"), "{contents}");
        assert!(position("Alpha") < position("Beta"), "{contents}");
        // The changed value is written in its shortest form, and the unchanged
        // one keeps its original text even though it'd be written shorter now.
        assert!(contents.contains("\"Transparency\": 0.6,"), "{contents}");
        assert!(contents.contains("1.100000023841858"), "{contents}");
    }

    #[test]
    fn scoped_instance_write_retargets_existing_jsonc_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project_path = temp.path().join("default.project.json");
        vfs.write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let instance_path = temp.path().join("Wire");
        vfs.create_dir_all(&instance_path).unwrap();
        vfs.write(
            instance_path.join("init.meta.jsonc"),
            r#"{
                // Existing JSONC metadata must not produce a competing JSON file.
                "className": "Part",
                "id": "wire-id",
                "properties": { "Transparency": 0.25 }
            }"#,
        )
        .unwrap();
        let root = InstanceSnapshot::new()
            .name("Wire")
            .class_name("Part")
            .properties(UstrMap::from_iter([(
                ustr("Transparency"),
                Variant::Float32(0.25),
            )]))
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(instance_path.clone())
                    .specified_id(Some(crate::RojoRef::new("wire-id".into())))
                    .middleware(Middleware::Dir),
            );
        let tree = RojoTree::new(root);
        let new_tree = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("Part")
                .with_name("Wire")
                .with_properties([(ustr("Transparency"), Variant::Float32(0.75))]),
        );
        let snapshot =
            syncback_instance(&vfs, &tree, &project, tree.get_root_id(), new_tree).unwrap();
        assert_eq!(
            snapshot.added_files(),
            vec![instance_path.join("init.meta.jsonc").as_path()]
        );
        snapshot.write_to_vfs(temp.path(), &vfs).unwrap();
        assert!(!instance_path.join("init.meta.json").exists());
        let contents = std::fs::read_to_string(instance_path.join("init.meta.jsonc")).unwrap();
        assert!(contents.starts_with("{\n    \"className\": \"Part\""));
        assert!(contents.contains("\"id\": \"wire-id\""));
        assert!(contents.contains("\"Transparency\": 0.75"));
    }

    /// A new sibling can shift which old instance each duplicate-named child
    /// pairs with, so the first `Part` inherits the old `Part`'s inline ID
    /// while the second one, which a Ref property points at, holds that same
    /// ID through its `Rojo_Id` attribute.
    #[test]
    fn inline_ids_never_duplicate_an_attribute_id() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let holder = src.join("Holder");
        std::fs::create_dir_all(&holder).unwrap();
        let project_path = temp.path().join("default.project.json");
        std::fs::write(
            &project_path,
            r#"{
                "name": "Test",
                "tree": { "$path": "src" },
                "syncbackRules": { "instancesAsDirectories": true }
            }"#,
        )
        .unwrap();
        std::fs::write(
            holder.join("init.meta.json"),
            r#"{
                "className": "Folder",
                "children": [
                    { "className": "Part", "name": "Part", "id": "shared" }
                ]
            }"#,
        )
        .unwrap();

        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let session = crate::serve_session::ServeSession::new(vfs, &project_path).unwrap();

        let mut new_tree = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
        let root = new_tree.insert(
            new_tree.root_ref(),
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("Holder"),
        );
        new_tree.insert(root, rbx_dom_weak::InstanceBuilder::new("Part"));
        let mut attributes = Attributes::new();
        attributes.insert(
            crate::REF_ID_ATTRIBUTE_NAME.into(),
            Variant::String("shared".into()),
        );
        let target = new_tree.insert(
            root,
            rbx_dom_weak::InstanceBuilder::new("Part").with_property("Attributes", attributes),
        );
        new_tree.insert(
            root,
            rbx_dom_weak::InstanceBuilder::new("ObjectValue")
                .with_property("Value", Variant::Ref(target)),
        );

        let snapshot = {
            let mut old_tree = session.tree();
            syncback_loop(
                session.vfs(),
                &mut old_tree,
                new_tree,
                session.root_project(),
            )
            .unwrap()
        };
        snapshot.write_to_vfs(temp.path(), session.vfs()).unwrap();

        let contents = std::fs::read_to_string(holder.join("init.meta.json")).unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let children = metadata["children"].as_array().unwrap();
        let ids: Vec<_> = children
            .iter()
            .map(|child| {
                (
                    child["id"].as_str(),
                    child["attributes"][crate::REF_ID_ATTRIBUTE_NAME].as_str(),
                )
            })
            .collect();
        assert_eq!(
            ids,
            vec![(None, None), (None, Some("shared")), (None, None)],
            "{contents}"
        );
        assert_eq!(
            children[2]["attributes"]["Rojo_Target_Value"].as_str(),
            Some("shared"),
            "{contents}"
        );
    }

    #[test]
    fn normalized_json_syncback_schedules_no_repeated_writes() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let project_path = temp.path().join("default.project.json");
        std::fs::write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#).unwrap();
        let input = || {
            let mut dom = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
            let attributes: Attributes = [("Zero".into(), Variant::Float64(-0.0))]
                .into_iter()
                .collect();
            let holder = dom.insert(
                dom.root_ref(),
                rbx_dom_weak::InstanceBuilder::new("Folder")
                    .with_name("Holder")
                    .with_property("Attributes", attributes),
            );
            dom.insert(
                holder,
                rbx_dom_weak::InstanceBuilder::new("NumberValue")
                    .with_name("Tiny")
                    .with_property("Value", Variant::Float64(0.0000005)),
            );
            dom.insert(
                holder,
                rbx_dom_weak::InstanceBuilder::new("Part")
                    .with_name("Solid")
                    .with_property("Transparency", Variant::Float32(0.0000005)),
            );
            dom
        };
        for pass in 0..2 {
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            let session = crate::serve_session::ServeSession::new(vfs, &project_path).unwrap();
            let snapshot = syncback_loop(
                session.vfs(),
                &mut session.tree(),
                input(),
                session.root_project(),
            )
            .unwrap();
            if pass == 0 {
                snapshot.write_to_vfs(temp.path(), session.vfs()).unwrap();
            } else {
                assert!(
                    snapshot.added_paths().is_empty(),
                    "{:?}",
                    snapshot.added_paths()
                );
                assert!(
                    snapshot.removed_paths().is_empty(),
                    "{:?}",
                    snapshot.removed_paths()
                );
            }
        }
        let binary = std::fs::File::open(src.join("Holder/Solid.rbxm")).unwrap();
        let dom = rbx_binary::from_reader(binary).unwrap();
        let part = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(
            part.properties.get(&ustr("Transparency")),
            Some(&Variant::Float32(0.0000005))
        );
    }

    #[test]
    fn json_model_fractional_attribute_edits_are_not_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let src = root.join("src");
        std::fs::create_dir(&src).unwrap();
        let project_path = root.join("default.project.json");
        std::fs::write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#).unwrap();
        for value in [-0.0000005, 0.01, 0.02] {
            let mut attributes = Attributes::new();
            attributes.insert("Amount".into(), Variant::Float64(value));
            let input = WeakDom::new(
                rbx_dom_weak::InstanceBuilder::new("Folder").with_child(
                    rbx_dom_weak::InstanceBuilder::new("NumberValue")
                        .with_name("Value")
                        .with_property("Attributes", attributes),
                ),
            );
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            let session = crate::serve_session::ServeSession::new(vfs, &project_path).unwrap();
            let snapshot = syncback_loop(
                session.vfs(),
                &mut session.tree(),
                input,
                session.root_project(),
            )
            .unwrap();
            let path = src.join("Value.model.json");
            assert!(snapshot.added_paths().contains(&path.as_path()));
            snapshot.write_to_vfs(&root, session.vfs()).unwrap();
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(saved["attributes"]["Amount"].as_f64(), Some(value.max(0.0)));
        }
    }

    #[test]
    fn directory_json_comparison_does_not_skip_tiny_binary_changes() {
        for (class, initial) in [
            ("Part", 0.0),
            ("Part", -0.0000005),
            ("NumberValue", 0.0),
            ("NumberValue", -0.0000005),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let holder = root.join("src/Holder");
            std::fs::create_dir_all(&holder).unwrap();
            let project_path = temp.path().join("default.project.json");
            std::fs::write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#).unwrap();
            let (property, old_value, value) = if class == "Part" {
                (
                    "Transparency",
                    Variant::Float32(initial as f32),
                    Variant::Float32(0.0000005),
                )
            } else {
                (
                    "Value",
                    Variant::Float64(initial),
                    Variant::Float64(0.0000005),
                )
            };
            let old = WeakDom::new(
                rbx_dom_weak::InstanceBuilder::new("Folder").with_child(
                    rbx_dom_weak::InstanceBuilder::new(class)
                        .with_name("Binary")
                        .with_property(property, old_value),
                ),
            );
            let binary_path = holder.join("Binary.rbxm");
            rbx_binary::to_writer(
                std::fs::File::create(&binary_path).unwrap(),
                &old,
                old.root().children(),
            )
            .unwrap();
            let mut input = WeakDom::new(rbx_dom_weak::InstanceBuilder::new("Folder"));
            let holder_ref = input.insert(
                input.root_ref(),
                rbx_dom_weak::InstanceBuilder::new("Folder").with_name("Holder"),
            );
            input.insert(
                holder_ref,
                rbx_dom_weak::InstanceBuilder::new(class)
                    .with_name("Binary")
                    .with_property(property, value.clone()),
            );
            let vfs = Vfs::new_default().unwrap();
            vfs.set_watch_enabled(false);
            let session = crate::serve_session::ServeSession::new(vfs, &project_path).unwrap();
            let snapshot = syncback_loop(
                session.vfs(),
                &mut session.tree(),
                input,
                session.root_project(),
            )
            .unwrap();
            assert!(snapshot.added_paths().contains(&binary_path.as_path()));
            snapshot.write_to_vfs(temp.path(), session.vfs()).unwrap();
            let saved =
                rbx_binary::from_reader(std::fs::File::open(&binary_path).unwrap()).unwrap();
            let inst = saved.get_by_ref(saved.root().children()[0]).unwrap();
            assert_eq!(inst.properties.get(&ustr(property)), Some(&value));
        }
    }

    fn rules(ignore_paths: &[&str], create_ignore_dir_paths: Option<bool>) -> SyncbackRules {
        SyncbackRules {
            ignore_trees: Vec::new(),
            ignore_paths: ignore_paths.iter().map(|s| s.to_string()).collect(),
            ignore_properties: IndexMap::new(),
            sync_current_camera: None,
            sync_unscriptable: None,
            ignore_referents: None,
            create_ignore_dir_paths,
            instances_as_directories: None,
            model_trees: Vec::new(),
        }
    }

    #[test]
    fn ignore_paths_negation() {
        let globs = Some(
            rules(&["**/*.lua", "!keep.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        // A later negation re-includes a path matched by an earlier pattern.
        assert!(!is_valid_path(&globs, base, Path::new("/test/foo.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep.lua")));
        // Paths matched by no rule are valid.
        assert!(is_valid_path(&globs, base, Path::new("/test/plain.txt")));
    }

    #[test]
    fn ignore_paths_negation_with_dir_expansion() {
        // With `create_ignore_dir_paths`, a negated `foo/**` pattern should also
        // re-include the `foo` directory itself, mirroring the file rule.
        let globs = Some(
            rules(&["**/*", "!keep/**"], Some(true))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!is_valid_path(&globs, base, Path::new("/test/drop/a.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep/a.lua")));
    }

    #[test]
    fn ignore_paths_escaped_bang_is_literal() {
        // `\!literal.lua` should ignore a file literally named `!literal.lua`
        // rather than being parsed as a negation.
        let globs = Some(
            rules(&[r"\!literal.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!globs.as_ref().unwrap()[0].is_negation());
        assert!(!is_valid_path(
            &globs,
            base,
            Path::new("/test/!literal.lua")
        ));
    }
}
