use indexmap::IndexMap;
use memofs::Vfs;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::{
    snapshot::{InstanceContext, InstanceWithMeta, RojoTree},
    snapshot_middleware::{default_sync_rules, Middleware},
    Project,
};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, Ustr, UstrMap, WeakDom,
};

use super::{
    file_names::{file_name_for_stem, stem_for_path},
    get_best_middleware, is_model_file, is_project_owned, name_for_inst,
    property_filter::filter_properties,
    ModelTrees,
};

#[derive(Clone, Copy)]
pub struct SyncbackData<'sync> {
    pub(super) vfs: &'sync Vfs,
    pub(super) old_tree: &'sync RojoTree,
    pub(super) new_tree: &'sync WeakDom,
    pub(super) project: &'sync Project,
    /// Whether this rewrites one instance's files for two-way sync, where the
    /// new tree only holds what that instance's own files store.
    pub(super) scoped: bool,
    /// Effective destination context inherited by instances without tracked sources.
    pub(super) context: Option<&'sync InstanceContext>,
    /// Instances in the new tree that are scripts or have script descendants,
    /// when they were computed ahead of time for the whole tree.
    pub(super) script_containers: Option<&'sync HashSet<Ref>>,
    /// Instances in the new tree that the `modelTrees` rules store as model
    /// files. Scoped rewrites keep the layout already on disk, so they have
    /// none.
    pub(super) model_trees: Option<&'sync ModelTrees>,
    /// IDs that instances in the new tree hold through their `Rojo_Id`
    /// attribute, so no other instance may be written with one of them.
    pub(super) attribute_ids: &'sync HashSet<String>,
}

pub struct SyncbackSnapshot<'sync> {
    pub data: SyncbackData<'sync>,
    pub old: Option<Ref>,
    pub new: Ref,
    pub path: PathBuf,
    pub middleware: Option<Middleware>,
}

impl<'sync> SyncbackSnapshot<'sync> {
    fn context(&self) -> Option<&'sync InstanceContext> {
        self.old_inst()
            .map(|old| &old.metadata().context)
            .or(self.data.context)
    }

    /// Constructs a SyncbackSnapshot from the provided refs
    /// while inheriting this snapshot's path and data. This should be used for
    /// directories.
    #[inline]
    pub fn with_joined_path(&self, new_ref: Ref, old_ref: Option<Ref>) -> anyhow::Result<Self> {
        let mut snapshot = Self {
            data: SyncbackData {
                context: self.context(),
                ..self.data
            },
            old: old_ref,
            new: new_ref,
            path: PathBuf::new(),
            middleware: None,
        };
        let middleware = get_best_middleware(&snapshot);
        let name = name_for_inst(middleware, snapshot.new_inst(), snapshot.old_inst())?;
        snapshot.path = self.path.join(name.as_ref());

        Ok(snapshot)
    }

    /// Constructs a SyncbackSnapshot inside `base_path` like
    /// [`Self::with_base_path`], but a new Instance is written using `stem`
    /// from [`unique_child_stems`] instead of its own name. Instances that
    /// already exist on the file system keep their current file name.
    ///
    /// [`unique_child_stems`]: super::file_names::unique_child_stems
    pub fn with_stem(
        &self,
        base_path: &Path,
        new_ref: Ref,
        old_ref: Option<Ref>,
        stem: &str,
    ) -> Self {
        let mut snapshot = Self {
            data: SyncbackData {
                context: self.context(),
                ..self.data
            },
            old: old_ref,
            new: new_ref,
            path: PathBuf::new(),
            middleware: None,
        };
        let old_source = snapshot
            .old_inst()
            .and_then(|old| old.metadata().relevant_paths.first())
            .and_then(|path| Some((path, path.file_name()?.to_str()?)));
        let name = match old_source {
            Some((source, name)) => super::file_names::canonical_script_source_name(
                get_best_middleware(&snapshot),
                source,
                &snapshot.old_inst().unwrap().metadata().context.sync_rules,
            )
            .map(|name| name.into_owned())
            .unwrap_or_else(|| name.to_owned()),
            None => file_name_for_stem(get_best_middleware(&snapshot), stem),
        };
        snapshot.path = base_path.join(name);

        snapshot
    }

    /// Returns the name this Instance should store in its metadata. This is
    /// only needed when its file name doesn't already give Rojo its real name.
    pub fn name_override(&self) -> Option<String> {
        let name = &self.new_inst().name;
        (self.file_stem() != name.as_str()).then(|| name.clone())
    }

    /// Returns the name Rojo will read this Instance's file back as, which is
    /// also the stem of its adjacent `.meta.json` file.
    pub fn file_stem(&self) -> &str {
        let middleware = get_best_middleware(self);
        let rules = self
            .context()
            .map(|context| context.sync_rules.as_slice())
            .unwrap_or(self.project().sync_rules.as_slice());
        for rule in rules.iter().chain(default_sync_rules()) {
            if rule.middleware == middleware && rule.matches(&self.path) {
                if let Ok(stem) = rule.file_name_for_path(&self.path) {
                    return stem;
                }
            }
        }

        stem_for_path(middleware, &self.path).unwrap_or(self.new_inst().name.as_str())
    }

    /// Constructs a SyncbackSnapshot from the provided refs and a base path,
    /// while inheriting this snapshot's data.
    ///
    /// The actual path of the snapshot is made by getting a file name for the
    /// snapshot and then appending it to the provided base path.
    #[inline]
    pub fn with_base_path(
        &self,
        base_path: &Path,
        new_ref: Ref,
        old_ref: Option<Ref>,
    ) -> anyhow::Result<Self> {
        let mut snapshot = Self {
            data: SyncbackData {
                context: self.context(),
                ..self.data
            },
            old: old_ref,
            new: new_ref,
            path: PathBuf::new(),
            middleware: None,
        };
        let middleware = get_best_middleware(&snapshot);
        let name = name_for_inst(middleware, snapshot.new_inst(), snapshot.old_inst())?;
        snapshot.path = base_path.join(name.as_ref());

        Ok(snapshot)
    }

    /// Constructs a SyncbackSnapshot with the provided path and refs while
    /// inheriting the data of the this snapshot.
    #[inline]
    pub fn with_new_path(&self, path: PathBuf, new_ref: Ref, old_ref: Option<Ref>) -> Self {
        Self {
            data: SyncbackData {
                context: self.context(),
                ..self.data
            },
            old: old_ref,
            new: new_ref,
            path,
            middleware: None,
        }
    }

    /// Allows a middleware to be 'forced' onto a SyncbackSnapshot to override
    /// the attempts to derive it.
    #[inline]
    pub fn middleware(mut self, middleware: Middleware) -> Self {
        self.middleware = Some(middleware);
        self
    }

    /// Returns a map of properties for an Instance from the 'new' tree
    /// with filtering done to avoid noise. This method filters out properties
    /// that are not meant to be present in Instances that are represented
    /// specially by a path, like `LocalScript.Source` and `StringValue.Value`.
    ///
    /// This method is not necessary or desired for blobs like Rbxm or non-path
    /// middlewares like JsonModel.
    #[inline]
    #[must_use]
    pub fn get_path_filtered_properties(&self, new_ref: Ref) -> Option<UstrMap<&'sync Variant>> {
        let inst = self.get_new_instance(new_ref)?;

        // The only filtering we have to do is filter out properties that are
        // special-cased in some capacity.
        let middleware = get_best_middleware(self);
        let properties = filter_properties(self.data.project, inst)
            .into_iter()
            .filter(|(name, _)| !filter_out_property(inst, name, middleware))
            .collect();

        Some(properties)
    }

    /// Returns a path to the provided Instance in the new DOM. This path is
    /// where you would look for the object in Roblox Studio.
    #[inline]
    pub fn get_new_inst_path(&self, referent: Ref) -> String {
        inst_path(self.new_tree(), referent)
    }

    /// Returns a path to the provided Instance in the old DOM. This path is
    /// where you would look for the object in Roblox Studio.
    #[inline]
    pub fn get_old_inst_path(&self, referent: Ref) -> String {
        inst_path(self.old_tree(), referent)
    }

    /// Returns an Instance from the old tree with the provided referent, if it
    /// exists.
    #[inline]
    pub fn get_old_instance(&self, referent: Ref) -> Option<InstanceWithMeta<'sync>> {
        self.data.old_tree.get_instance(referent)
    }

    /// Returns an Instance from the new tree with the provided referent, if it
    /// exists.
    #[inline]
    pub fn get_new_instance(&self, referent: Ref) -> Option<&'sync Instance> {
        self.data.new_tree.get_by_ref(referent)
    }

    /// The 'old' Instance this snapshot is for, if it exists.
    #[inline]
    pub fn old_inst(&self) -> Option<InstanceWithMeta<'sync>> {
        self.old
            .and_then(|old| self.data.old_tree.get_instance(old))
    }

    /// The 'new' Instance this snapshot is for.
    #[inline]
    pub fn new_inst(&self) -> &'sync Instance {
        self.data
            .new_tree
            .get_by_ref(self.new)
            .expect("SyncbackSnapshot should not contain invalid referents")
    }

    /// Identifies a tracked file rename without treating fixed project paths as movable.
    pub fn is_script_suffix_migration(&self, path: &Path) -> bool {
        self.old_inst().is_some_and(|old| {
            let Some(crate::snapshot::InstigatingSource::Path(source)) =
                &old.metadata().instigating_source
            else {
                return false;
            };
            source != path
                && source.parent() == path.parent()
                && super::file_names::canonical_script_source_name(
                    get_best_middleware(self),
                    source,
                    &old.metadata().context.sync_rules,
                )
                .is_some_and(|name| {
                    path.file_name().and_then(|name| name.to_str()) == Some(name.as_ref())
                })
        })
    }

    /// A filtered rename must not remove its source without installing its replacement.
    pub fn validate_script_migration(&self, old: &Path, new: &Path) -> anyhow::Result<()> {
        let patterns = self
            .project()
            .syncback_rules
            .as_ref()
            .map(super::SyncbackRules::compile_globs)
            .transpose()?;
        for path in [old, new] {
            anyhow::ensure!(
                super::is_valid_path(&patterns, self.project().folder_location(), path),
                "Cannot migrate a script source because an ignore pattern excludes {}",
                path.display()
            );
        }
        Ok(())
    }

    fn validate_script_rule(&self, path: &Path) -> anyhow::Result<()> {
        let middleware = get_best_middleware(self);
        if middleware.is_dir() || self.middleware.is_some() {
            return Ok(());
        }
        let context = self.context();
        let rules = context
            .map(|context| context.sync_rules.as_slice())
            .unwrap_or(self.project().sync_rules.as_slice());
        let emit_legacy = context
            .map(|context| context.emit_legacy_scripts)
            .or(self.project().emit_legacy_scripts)
            .unwrap_or_else(|| crate::snapshot::InstanceContext::default().emit_legacy_scripts);
        let rule = rules
            .iter()
            .chain(default_sync_rules())
            .find(|rule| rule.matches(path))
            .ok_or_else(|| {
                anyhow::anyhow!("No sync rule matches script destination {}", path.display())
            })?;
        let reader = match rule.middleware {
            Middleware::ServerScript if emit_legacy => Middleware::LegacyServerScript,
            Middleware::ClientScript if emit_legacy => Middleware::LegacyClientScript,
            other => other,
        };
        let suffix = super::file_names::script_kind_suffix(middleware);
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|stem| suffix.and_then(|suffix| stem.strip_suffix(suffix)));
        anyhow::ensure!(
            suffix.is_some()
                && super::file_names::script_kind_suffix(reader) == suffix
                && rule.file_name_for_path(path).ok() == stem,
            "Cannot write a script destination with an incompatible sync rule at {}",
            path.display()
        );
        Ok(())
    }

    /// Refuses to install a renamed source over a file the old tree does not own.
    pub fn validate_script_destination(&self, path: &Path) -> anyhow::Result<()> {
        if self
            .old_inst()
            .as_ref()
            .and_then(super::source_file)
            .is_some_and(|source| source == path)
        {
            return Ok(());
        }
        match self.vfs().metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.validate_script_rule(path)
            }
            Err(error) => return Err(error.into()),
        }
        if self.middleware.is_some()
            && self.old_inst().is_some_and(|old| {
                old.metadata()
                    .relevant_paths
                    .iter()
                    .any(|source| source == path)
            })
        {
            return Ok(());
        }
        let owned = self.old_tree().descendants().any(|instance| {
            self.get_old_instance(instance.referent())
                .as_ref()
                .and_then(super::source_file)
                .is_some_and(|source| source == path)
        });
        anyhow::bail!(
            "Cannot write script source because {} already exists at {}",
            if owned {
                "another managed source"
            } else {
                "an unmanaged file"
            },
            path.display()
        )
    }

    /// Returns the root Project that was used to make this snapshot.
    #[inline]
    pub fn project(&self) -> &'sync Project {
        self.data.project
    }

    /// Returns whether this syncback rewrites a single instance for two-way
    /// sync rather than reconciling a whole place.
    #[inline]
    pub fn is_scoped(&self) -> bool {
        self.data.scoped
    }

    /// Returns whether the provided Instance in the new tree is a script or
    /// has a script descendant.
    pub fn contains_script(&self, new_ref: Ref) -> bool {
        match self.data.script_containers {
            Some(containers) => containers.contains(&new_ref),
            None => super::inline::contains_script(self.new_tree(), new_ref),
        }
    }

    /// Returns the model middleware a `modelTrees` rule assigns to this
    /// snapshot's Instance. Sources named by a project node keep the path and
    /// format the project gives them, and a nested project file stays one.
    pub fn model_tree_middleware(&self) -> Option<Middleware> {
        let middleware = self.data.model_trees?.middleware(self.new)?;
        let from_project = self
            .old_inst()
            .is_some_and(|old| is_project_owned(old.metadata()));
        (!from_project).then_some(middleware)
    }

    /// Returns whether the provided Instance in the new tree is stored as a
    /// model file by a `modelTrees` rule, or has a descendant that is.
    pub fn contains_model_tree(&self, new_ref: Ref) -> bool {
        self.data
            .model_trees
            .is_some_and(|trees| trees.contains(new_ref))
    }

    /// Returns whether `new_ref` can be written over the files of `old`. A
    /// model file and any other layout have different paths on disk, so when
    /// only one side is a model file, or their formats differ, the old files
    /// are removed and the Instance is written as if it were new.
    pub fn keeps_model_layout(&self, new_ref: Ref, old: InstanceWithMeta<'sync>) -> bool {
        let Some(old_middleware) = old.metadata().middleware else {
            return true;
        };
        let paired = self.with_new_path(PathBuf::new(), new_ref, Some(old.id()));
        let new_middleware = get_best_middleware(&paired);
        new_middleware == old_middleware
            || !(is_model_file(new_middleware) || is_model_file(old_middleware))
    }

    /// Returns an error if replacing the files of `old` with another layout
    /// would remove a path the project's `ignorePaths` rule excludes. Moving
    /// between a model file and a directory removes everything the old layout
    /// stored, which would take the excluded files with it.
    pub fn validate_layout_change(&self, old: InstanceWithMeta<'sync>) -> anyhow::Result<()> {
        let Some(rules) = &self.project().syncback_rules else {
            return Ok(());
        };
        let globs = Some(rules.compile_globs()?);
        let base = self.project().folder_location();
        // The directory is removed with everything in it, including files
        // Rojo doesn't turn into Instances, so this walks the disk itself
        // rather than the old tree.
        let mut pending: Vec<PathBuf> = old.metadata().relevant_paths.clone();
        while let Some(path) = pending.pop() {
            let metadata = match fs_err::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !super::is_valid_path(&globs, base, &path) {
                return Err(super::IgnoreConflict(format!(
                    "Cannot change how {} is stored because the project's ignorePaths rule \
                     excludes {}",
                    self.get_old_inst_path(old.id()),
                    path.display()
                ))
                .into());
            }
            if metadata.is_dir() {
                for entry in fs_err::read_dir(&path)? {
                    pending.push(entry?.path());
                }
            }
        }
        Ok(())
    }

    /// Returns the Instances the `modelTrees` rules store as model files,
    /// when this is a full syncback.
    #[inline]
    pub fn model_trees(&self) -> Option<&'sync ModelTrees> {
        self.data.model_trees
    }

    /// Returns whether an instance in the new tree holds `id` through its
    /// `Rojo_Id` attribute.
    #[inline]
    pub fn is_attribute_id(&self, id: &str) -> bool {
        self.data.attribute_ids.contains(id)
    }

    /// Returns the underlying VFS being used for syncback.
    #[inline]
    pub fn vfs(&self) -> &'sync Vfs {
        self.data.vfs
    }

    /// Returns the WeakDom used for the 'new' tree.
    #[inline]
    pub fn new_tree(&self) -> &'sync WeakDom {
        self.data.new_tree
    }

    /// Returns the WeakDom used for the 'old' tree.
    #[inline]
    pub fn old_tree(&self) -> &'sync WeakDom {
        self.data.old_tree.inner()
    }

    /// Returns user-specified property ignore rules.
    #[inline]
    pub fn ignore_props(&self) -> Option<&IndexMap<Ustr, Vec<Ustr>>> {
        self.data
            .project
            .syncback_rules
            .as_ref()
            .map(|rules| &rules.ignore_properties)
    }

    /// Returns user-specified ignore tree.
    #[inline]
    pub fn ignore_tree(&self) -> Option<&[String]> {
        self.data
            .project
            .syncback_rules
            .as_ref()
            .map(|rules| rules.ignore_trees.as_slice())
    }
}

pub fn filter_out_property(inst: &Instance, prop_name: &str, middleware: Middleware) -> bool {
    match inst.class.as_str() {
        "Script" | "LocalScript" | "ModuleScript" => {
            // These properties shouldn't be set by scripts that are created via
            // `$path` or via being on the file system.
            prop_name == "Source" || prop_name == "ScriptGuid"
        }
        "LocalizationTable" => prop_name == "Contents",
        // A StringValue with children has no text file to hold its value, so
        // keep it in the directory metadata. Text files and project nodes own
        // their values instead.
        "StringValue" => middleware != Middleware::Dir && prop_name == "Value",
        _ => false,
    }
}

pub fn inst_path(dom: &WeakDom, referent: Ref) -> String {
    dom.full_path_of(referent, "/")
}

#[cfg(test)]
mod test {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    use super::inst_path as inst_path_outer;

    #[test]
    fn inst_path() {
        let mut new_tree = WeakDom::new(InstanceBuilder::new("ROOT"));

        let child_1 = new_tree.insert(new_tree.root_ref(), InstanceBuilder::new("Child1"));
        let child_2 = new_tree.insert(child_1, InstanceBuilder::new("Child2"));
        let child_3 = new_tree.insert(child_2, InstanceBuilder::new("Child3"));

        assert_eq!(inst_path_outer(&new_tree, new_tree.root_ref()), "");
        assert_eq!(inst_path_outer(&new_tree, child_1), "Child1");
        assert_eq!(inst_path_outer(&new_tree, child_2), "Child1/Child2");
        assert_eq!(inst_path_outer(&new_tree, child_3), "Child1/Child2/Child3");
    }
}
