mod file_names;
mod fs_snapshot;
mod hash;
mod inline;
mod property_filter;
mod ref_properties;
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
    snapshot::{InstanceWithMeta, RojoTree},
    snapshot_middleware::{dir_init_files, Middleware},
    syncback::ref_properties::{collect_referents, link_referents},
    Project,
};

pub use file_names::{
    directory_middleware_for_instance, extension_for_middleware, name_for_inst, sibling_order_key,
    unique_child_stems, validate_file_name,
};
pub use fs_snapshot::FsSnapshot;
pub use hash::*;
pub(crate) use property_filter::can_write_property;
pub use property_filter::{filter_properties, filter_properties_preallocated, is_json_compatible};
pub use snapshot::{SyncbackData, SyncbackSnapshot};
pub use write_decision::{decide_property_write, PropertyWriteDecision};

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
    if matches!(
        middleware,
        Middleware::Rbxm | Middleware::Rbxmx | Middleware::JsonModel
    ) {
        anyhow::bail!("Model-file updates require subtree serialization");
    }
    let snapshot = SyncbackSnapshot {
        data: SyncbackData {
            vfs,
            old_tree,
            new_tree: &new_tree,
            project,
            scoped: true,
            script_containers: None,
        },
        old: Some(id),
        new: new_tree.root_ref(),
        path: path.clone(),
        middleware: Some(middleware),
    };
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
    let mut output = middleware.syncback(&snapshot)?.fs_snapshot;

    // Directory scripts are serialized to the default initializer name, but
    // the directory may use another one, like `init.lua`. Writing both would
    // be a multi-file change and leave two competing initializers.
    if let Some(tracked) = source_file(&old_inst).filter(|tracked| *tracked != path) {
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

pub fn syncback_loop(
    vfs: &Vfs,
    old_tree: &mut RojoTree,
    mut new_tree: WeakDom,
    project: &Project,
) -> anyhow::Result<FsSnapshot> {
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
        link_referents(deferred_referents, &mut new_tree)?;
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

    let project_path = project.folder_location();

    let relayout = if instances_as_directories(project) {
        inline::needs_relayout(old_tree)
    } else {
        HashSet::new()
    };
    let script_containers = inline::script_containers(&new_tree);
    let syncback_data = SyncbackData {
        vfs,
        old_tree,
        new_tree: &new_tree,
        project,
        scoped: false,
        script_containers: Some(&script_containers),
    };

    let mut snapshots = vec![SyncbackSnapshot {
        data: syncback_data,
        old: Some(old_tree.get_root_id()),
        new: new_tree.root_ref(),
        path: project.file_location.clone(),
        middleware: Some(Middleware::Project),
    }];

    let mut fs_snapshot = FsSnapshot::new();

    'syncback: while let Some(snapshot) = snapshots.pop() {
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

        if !is_valid_path(&ignore_patterns, project_path, &snapshot.path) {
            log::debug!("Skipping {inst_path} because its path matches ignore pattern");
            continue;
        }
        if let Some(syncback_rules) = &project.syncback_rules {
            // Ignore trees;
            for ignored in &syncback_rules.ignore_trees {
                if inst_path.starts_with(ignored.as_str()) {
                    log::debug!("Tree {inst_path} is blocked by project");
                    continue 'syncback;
                }
            }
        }

        let middleware = get_best_middleware(&snapshot);

        log::trace!(
            "Middleware for {inst_path} is {:?} (path is {})",
            middleware,
            snapshot.path.display()
        );

        if matches!(middleware, Middleware::Json | Middleware::Toml) {
            log::warn!("Cannot syncback {middleware:?} at {inst_path}, skipping");
            continue;
        }

        let syncback = match middleware.syncback(&snapshot) {
            Ok(syncback) => syncback,
            Err(err) if middleware == Middleware::Dir => {
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
                let new_snapshot = snapshot.with_new_path(path, snapshot.new, snapshot.old);
                log::warn!(
                    "Could not syncback {inst_path} as a Directory because: {err}.\n\
                    It will instead be synced back as a {new_middleware:?}."
                );
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
                // Instances stored inside a parent's metadata have no files of
                // their own and disappear when that metadata is rewritten.
                let Some(source) = inst.metadata().instigating_source.as_ref() else {
                    continue;
                };
                let path = source.path();
                let inst_path = snapshot.get_old_inst_path(inst.id());
                if !is_valid_path(&ignore_patterns, project_path, path) {
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
            }
        }

        // TODO provide replacement snapshots for e.g. two way sync

        fs_snapshot.merge_with_filter(syncback.fs_snapshot, |path| {
            is_valid_path(&ignore_patterns, project_path, path)
        });

        snapshots.extend(syncback.children);
    }

    Ok(fs_snapshot)
}

pub struct SyncbackReturn<'sync> {
    pub fs_snapshot: FsSnapshot,
    pub children: Vec<SyncbackSnapshot<'sync>>,
    pub removed_children: Vec<InstanceWithMeta<'sync>>,
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
        middleware = match middleware {
            Middleware::ServerScript => Middleware::ServerScriptDir,
            Middleware::ClientScript => Middleware::ClientScriptDir,
            Middleware::PluginScript => Middleware::PluginScriptDir,
            Middleware::ModuleScript => Middleware::ModuleScriptDir,
            Middleware::Csv => Middleware::CsvDir,
            Middleware::JsonModel | Middleware::Text => Middleware::Dir,
            _ => middleware,
        }
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
    ignore_trees: Vec<String>,
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
            set.extend(list)
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
