use anyhow::Context as _;
use crossbeam_channel::{select, Receiver, RecvError, Sender};
use jod_thread::JoinHandle;
use memofs::{IoResultExt, Vfs, VfsEvent};
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    ustr, HashMapExt as _, InstanceBuilder, Ustr, UstrMap, WeakDom,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{
    message_queue::MessageQueue,
    project::Project,
    resolution::normalize_json_variant,
    rojo_ref::merge_link_attributes,
    snapshot::{
        apply_patch_set, compute_patch_set, compute_saved_patch_set, compute_shallow_patch_set,
        AppliedPatchSet, AppliedPatchUpdate, InstanceSnapshot, InstanceWithMeta, InstigatingSource,
        PatchAdd, PatchSet, PatchUpdate, RojoTree, SnapshotReferents,
    },
    snapshot_middleware::{
        snapshot_from_vfs, snapshot_project_node, snapshot_shallow_from_vfs, Middleware,
    },
    syncback::{
        can_write_property, decide_property_write, drop_duplicate_ids, drop_ignored_properties,
        get_property_filter, same_value, source_file, stale_layouts, syncback_instance,
        syncback_model_file, syncback_new_subtree, validate_write_targets, CompiledModelTreeRules,
        FsSnapshot, PropertyWriteDecision,
    },
    web::interface::{AddedIds, InstanceAddition, WriteAuthority, WriteError, WriteResult},
    REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX,
};

/// How many times a Studio-authoritative write is retried when its file keeps
/// changing between the read and the write.
const STUDIO_WRITE_ATTEMPTS: u32 = 3;

/// A mutation and the channel dedicated to its own result.
pub(crate) struct TreeMutation {
    pub message_cursor: Option<u32>,
    pub authority: WriteAuthority,
    pub patch_set: PatchSet,
    /// Subtrees created in Studio, keyed by the id of each root.
    pub additions: HashMap<Ref, InstanceAddition>,
    pub result_sender: Sender<WriteResult>,
}

/// Processes file change events, updates the DOM, and sends those updates
/// through a channel for other stuff to consume.
///
/// Owns the connection between Rojo's VFS and its DOM by holding onto another
/// thread that processes messages.
///
/// Consumers of ChangeProcessor, like ServeSession, are intended to communicate
/// with this object via channels.
///
/// ChangeProcessor expects to be the only writer to the RojoTree and Vfs
/// objects passed to it.
pub struct ChangeProcessor {
    /// Controls the runtime of the processor thread. When signaled, the job
    /// thread will finish its current work and terminate.
    ///
    /// This channel should be signaled before dropping ChangeProcessor or we'll
    /// hang forever waiting for the message processing loop to terminate.
    shutdown_sender: Sender<()>,

    /// A handle to the message processing thread. When dropped, we'll block
    /// until it's done.
    ///
    /// Allowed to be unused because dropping this value has side effects.
    #[allow(unused)]
    job_thread: JoinHandle<Result<(), RecvError>>,
}

impl ChangeProcessor {
    /// Spin up the ChangeProcessor, connecting it to the given tree, VFS, and
    /// outbound message queue.
    pub fn start(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs>,
        message_queue: Arc<MessageQueue>,
        tree_mutation_receiver: Receiver<TreeMutation>,
        pool: Option<Arc<rayon::ThreadPool>>,
        project: Arc<Project>,
    ) -> Self {
        let (shutdown_sender, shutdown_receiver) = crossbeam_channel::bounded(1);
        let vfs_receiver = vfs.event_receiver();
        let task = JobThreadContext {
            tree,
            vfs,
            message_queue,
            project: project.into(),
        };

        let job_thread = jod_thread::Builder::new()
            .name("ChangeProcessor thread".to_owned())
            .spawn(move || {
                log::trace!("ChangeProcessor thread started");

                loop {
                    select! {
                        recv(vfs_receiver) -> event => {
                            let event = event?;
                            match &pool {
                                Some(pool) => pool.install(|| task.handle_vfs_event(event)),
                                None => task.handle_vfs_event(event),
                            }
                        },
                        recv(tree_mutation_receiver) -> mutation => {
                            let mutation = mutation?;
                            let result = task.handle_write(
                                mutation.patch_set,
                                &mutation.additions,
                                mutation.message_cursor,
                                mutation.authority,
                            );
                            let _ = mutation.result_sender.send(result);
                        },
                        recv(shutdown_receiver) -> _ => {
                            log::trace!("ChangeProcessor shutdown signal received...");
                            return Ok(());
                        },
                    }
                }
            })
            .expect("Could not start ChangeProcessor thread");

        Self {
            shutdown_sender,
            job_thread,
        }
    }
}

impl Drop for ChangeProcessor {
    fn drop(&mut self) {
        // Signal the job thread to start spinning down. Without this we'll hang
        // forever waiting for the thread to finish its infinite loop.
        let _ = self.shutdown_sender.send(());

        // After this function ends, the job thread will be joined. It might
        // block for a small period of time while it processes its last work.
    }
}

/// Contains all of the state needed to synchronize the DOM and VFS.
struct JobThreadContext {
    /// A handle to the DOM we're managing.
    tree: Arc<Mutex<RojoTree>>,

    /// A handle to the VFS we're managing.
    vfs: Arc<Vfs>,

    /// Whenever changes are applied to the DOM, we should push those changes
    /// into this message queue to inform any connected clients.
    message_queue: Arc<MessageQueue>,

    /// The project's serialization and filtering rules, read again whenever
    /// the project file changes.
    project: LiveProject,
}

/// The project as it is now, and what two-way sync knows about how to apply a
/// change to its rules.
struct LiveProject {
    project: Mutex<Arc<Project>>,
    /// The authority of the latest write, which is the only time Studio says
    /// which side wins. `None` until the first write.
    authority: Mutex<Option<WriteAuthority>>,
    /// Whether the `modelTrees` rules changed before any write said which
    /// side wins, so the first write decides whether to apply them.
    relayout_pending: std::sync::atomic::AtomicBool,
    /// While a relayout runs, what it wrote, so undoing it touches exactly
    /// that, even when the tree never learned about it.
    receipts: Mutex<Option<WriteReceipts>>,
}

/// What a relayout wrote.
#[derive(Default)]
struct WriteReceipts {
    /// Every path it installed.
    installed: Vec<PathBuf>,
    /// Every existing file it rewrote, with what it wrote, or `None` if it
    /// removed the file.
    rewritten: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl From<Arc<Project>> for LiveProject {
    fn from(project: Arc<Project>) -> Self {
        Self {
            project: Mutex::new(project),
            authority: Mutex::new(None),
            relayout_pending: Default::default(),
            receipts: Mutex::new(None),
        }
    }
}

impl JobThreadContext {
    fn project(&self) -> Arc<Project> {
        Arc::clone(&self.project.project.lock().unwrap())
    }

    /// Computes and applies patches to the DOM for a given file path.
    ///
    /// This function finds the nearest ancestor to the given path that has associated instances
    /// in the tree.
    /// It then computes and applies changes for each affected instance ID and
    /// returns a vector of applied patch sets.
    #[cfg(test)]
    fn apply_patches(&self, path: PathBuf) -> Vec<AppliedPatchSet> {
        self.apply_patches_with_publication(path, |_, _| {})
    }

    #[cfg(test)]
    fn apply_patches_with_publication(
        &self,
        path: PathBuf,
        before_publication: impl FnOnce(&RojoTree, &[AppliedPatchSet]),
    ) -> Vec<AppliedPatchSet> {
        self.try_apply_patches_with_publication(path, before_publication)
            .unwrap_or_else(|error| {
                log::error!("Cannot refresh project import mappings. {error:#}");
                Vec::new()
            })
    }

    #[profiling::function]
    fn try_apply_patches_with_publication(
        &self,
        path: PathBuf,
        before_publication: impl FnOnce(&RojoTree, &[AppliedPatchSet]),
    ) -> anyhow::Result<Vec<AppliedPatchSet>> {
        let mut tree = {
            profiling::scope!("Rojo.WaitForTree");
            self.tree.lock().unwrap()
        };
        let mut patches = Vec::new();
        let mut source_count = 0;
        self.for_each_fs_change(&mut tree, &path, |_, patch| {
            source_count += 1;
            if !patch.added_instances.is_empty()
                || !patch.removed_instances.is_empty()
                || !patch.updated_instances.is_empty()
            {
                patches.push(patch);
            }
        });
        let require_context = tree
            .root()
            .metadata()
            .context
            .require_context
            .clone()
            .filter(|_| requires_mapping_transaction(&tree, &patches, source_count));
        let apply = |tree: &mut RojoTree| {
            profiling::scope!("Rojo.ApplyFilesystemPatches");
            if source_count <= 1 || patches.is_empty() {
                patches
                    .into_iter()
                    .map(|patch| apply_patch_set(tree, patch))
                    .filter(|patch| !patch.is_empty())
                    .collect::<Vec<_>>()
            } else {
                // Overlapping mounts must compute each diff against the result of the
                // previous one, not apply stale patches prepared against the old tree.
                let mut applied = Vec::new();
                self.for_each_fs_change(tree, &path, |tree, patch| {
                    let patch = apply_patch_set(tree, patch);
                    if !patch.is_empty() {
                        applied.push(patch);
                    }
                });
                applied
            }
        };
        let applied = if let Some(require_context) = require_context {
            // Validate mapping changes on a candidate before publishing any part of them.
            // Ordinary property changes and no-op events never copy the project tree.
            let mut candidate = {
                profiling::scope!("Rojo.CloneMappingCandidate");
                tree.clone_for_serialization()
            };
            let mut applied = apply(&mut candidate);
            if let Some(refresh) =
                self.refresh_require_context(&mut candidate, &require_context.project_root)?
            {
                if !refresh.is_empty() {
                    applied.push(refresh);
                    applied = vec![AppliedPatchSet::combine(applied)];
                }
            }
            *tree = candidate;
            applied
        } else {
            apply(&mut tree)
        };
        before_publication(&tree, &applied);
        // Readers must not observe the changed DOM before its revision is published.
        {
            profiling::scope!("Rojo.PublishFilesystemChanges");
            self.message_queue.push_messages(&applied);
        }
        Ok(applied)
    }

    #[profiling::function]
    fn refresh_require_context(
        &self,
        tree: &mut RojoTree,
        project_root: &Path,
    ) -> anyhow::Result<Option<AppliedPatchSet>> {
        fn snapshot(tree: &RojoTree, id: Ref) -> InstanceSnapshot {
            let instance = tree.get_instance(id).unwrap();
            InstanceSnapshot::new()
                .snapshot_id(id)
                .name(instance.name())
                .class_name(instance.class_name())
                .properties(instance.properties().clone())
                .metadata(instance.metadata().clone())
                .children(
                    instance
                        .children()
                        .iter()
                        .map(|child| snapshot(tree, *child))
                        .collect::<Vec<_>>(),
                )
        }
        let context = crate::serve_session::build_require_context_from_tree(tree, project_root)?;
        if tree.root().metadata().context.require_context.as_deref() == Some(context.as_ref()) {
            return Ok(None);
        }
        let mut snapshot = {
            profiling::scope!("Rojo.SnapshotChangedMappings");
            snapshot(tree, tree.get_root_id())
        };
        {
            profiling::scope!("Rojo.TransformChangedImports");
            crate::snapshot_middleware::prepare_requires(&mut snapshot, &self.vfs, &context)?;
        }
        let mut pending = vec![&snapshot];
        while let Some(instance) = pending.pop() {
            anyhow::ensure!(
                instance.metadata.context.reserved_cache_path().is_none()
                    || instance
                        .metadata
                        .context
                        .require_context
                        .as_ref()
                        .is_some_and(|current| {
                            Arc::ptr_eq(current, &context) || current.as_ref() == context.as_ref()
                        }),
                "Cannot refresh imports for \"{}\". Keeping the previous project revision",
                instance.name
            );
            pending.extend(&instance.children);
        }
        // Mapping and source-transform baselines are server-only. Refresh them
        // without publishing unchanged client metadata as another revision.
        let mut applied = AppliedPatchSet::new();
        let mut pending = vec![&snapshot];
        while let Some(instance) = pending.pop() {
            let id = instance.snapshot_id;
            if let Some(source) = instance.properties.get(&ustr("Source")) {
                if tree
                    .get_instance(id)
                    .unwrap()
                    .properties()
                    .get(&ustr("Source"))
                    != Some(source)
                {
                    tree.get_instance_mut(id)
                        .unwrap()
                        .properties_mut()
                        .insert(ustr("Source"), source.clone());
                    applied.updated.push(AppliedPatchUpdate {
                        changed_properties: [(ustr("Source"), Some(source.clone()))]
                            .into_iter()
                            .collect(),
                        ..AppliedPatchUpdate::new(id)
                    });
                }
            }
            tree.update_metadata(id, instance.metadata.clone());
            pending.extend(&instance.children);
        }
        Ok(Some(applied))
    }

    #[profiling::function]
    fn for_each_fs_change(
        &self,
        tree: &mut RojoTree,
        path: &Path,
        mut process: impl FnMut(&mut RojoTree, PatchSet),
    ) {
        // Find the nearest ancestor to this path that has
        // associated instances in the tree. This helps make sure
        // that we handle additions correctly, especially if we
        // receive events for descendants of a large tree being
        // created all at once.
        let mut current_path = path;
        let affected_ids = loop {
            let ids = tree.get_ids_at_path(current_path);

            log::trace!("Path {} affects IDs {:?}", current_path.display(), ids);

            if !ids.is_empty() {
                break ids.to_vec();
            }

            log::trace!("Trying parent path...");
            match current_path.parent() {
                Some(parent) => current_path = parent,
                None => break Vec::new(),
            }
        };

        // Something new in a folder only adds that one entry, so only it is
        // read, instead of the folder's whole subtree.
        if let [id] = affected_ids[..] {
            if current_path != path {
                if let Some(patch) = self.new_entry_patch(tree, id, current_path, path) {
                    process(tree, patch);
                    return;
                }
            }
        }

        for id in affected_ids {
            // A change to one of an instance's own files, like a directory's
            // init.meta.json or init.luau, can only change that instance and
            // what its metadata stores. Re-reading the directory's whole subtree
            // for it would make every edit to a large folder take minutes.
            let own_file = current_path == path
                && tree
                    .get_metadata(id)
                    .and_then(|metadata| metadata.instigating_source.as_ref())
                    .is_some_and(|source| source.path() != current_path);
            if let Some(patch) = compute_changes(tree, &self.vfs, id, own_file) {
                process(tree, patch);
            }
        }
    }

    #[profiling::function]
    fn handle_vfs_event(&self, event: VfsEvent) {
        log::trace!("Vfs event: {:?}", event);

        // Update the VFS immediately with the event.
        {
            profiling::scope!("Rojo.CommitVfsEvent");
            self.vfs
                .commit_event(&event)
                .expect("Error applying VFS change");
        }

        // The project's state directory holds journals and staged files, which
        // come and go during a write and are never part of the tree.
        if self.is_in_state_directory(&event) {
            return;
        }

        // A `.gitkeep` is never an instance, so its own events change nothing.
        // Anything else that appears makes the `.gitkeep`s above it unneeded.
        match &event {
            VfsEvent::Create(path) | VfsEvent::Write(path) | VfsEvent::Remove(path)
                if is_keep_file(path) =>
            {
                return;
            }
            VfsEvent::Create(path) => {
                if let Some(directory) = path.parent() {
                    self.remove_stale_keep_files(directory);
                }
            }
            _ => {}
        }

        let project_file_changed = matches!(
            &event,
            VfsEvent::Create(path) | VfsEvent::Write(path) if self.is_project_file(path)
        );

        // For a given VFS event, we might have many changes to different parts
        // of the tree. Calculate and apply all of these changes.
        let applied = match event {
            VfsEvent::Create(path) | VfsEvent::Write(path) => self
                .try_apply_patches_with_publication(
                    canonicalize_or_original(&self.vfs, path),
                    |_, _| {},
                ),
            VfsEvent::Remove(path) => match (path.parent(), path.file_name()) {
                (Some(parent), Some(file_name)) => {
                    let parent_normalized =
                        canonicalize_or_original(&self.vfs, parent.to_path_buf());
                    self.try_apply_patches_with_publication(
                        parent_normalized.join(file_name),
                        |_, _| {},
                    )
                }
                _ => {
                    log::warn!("Cannot identify the removed path");
                    Ok(Vec::new())
                }
            },
            _ => {
                log::warn!("Unhandled VFS event: {:?}", event);
                Ok(Vec::new())
            }
        };
        if let Err(error) = applied {
            log::error!("Cannot refresh project import mappings. {error:#}");
            return;
        }

        if project_file_changed {
            self.reload_project();
        }
    }

    /// Handles a change to `path`, which no instance comes from yet, under the
    /// folder `directory` that `parent` comes from, by reading only the entry
    /// of `directory` that holds `path` and adding it. Returns `None` when the
    /// entry could change the folder itself, like an `init` file, which needs
    /// the folder read again.
    #[profiling::function]
    fn new_entry_patch(
        &self,
        tree: &RojoTree,
        parent: Ref,
        directory: &Path,
        path: &Path,
    ) -> Option<PatchSet> {
        let metadata = tree.get_metadata(parent)?;
        let is_folder = matches!(
            &metadata.instigating_source,
            Some(InstigatingSource::Path(source)) if source == directory
        ) && metadata
            .middleware
            .is_some_and(|middleware| middleware.is_dir());
        if !is_folder {
            return None;
        }
        let name = path.strip_prefix(directory).ok()?.components().next()?;
        let entry = directory.join(name);
        let name = name.as_os_str().to_str()?;
        if changes_folder(name) || !tree.get_ids_at_path(&entry).is_empty() {
            return None;
        }

        let mut context = metadata.context.clone();
        if crate::snapshot::is_path_ignored(&context.path_ignore_rules, &entry) {
            return Some(PatchSet::new());
        }
        // Children are read with their folder on the stack, like a full read
        // of the folder does, so a link back to it is still caught.
        let canonical = canonicalize_or_original(&self.vfs, directory.to_path_buf());
        Arc::make_mut(
            context
                .active_directories
                .get_or_insert_with(Default::default),
        )
        .push(canonical);
        let snapshot = match snapshot_from_vfs(&context, &self.vfs, &entry) {
            Ok(Some(snapshot)) => snapshot,
            // Gone again, or not something Rojo reads.
            Ok(None) => return Some(PatchSet::new()),
            Err(error) => {
                log::error!("Snapshot error: {error:?}");
                return Some(PatchSet::new());
            }
        };
        Some(PatchSet {
            added_instances: vec![PatchAdd {
                parent_id: parent,
                instance: snapshot,
            }],
            ..PatchSet::new()
        })
    }

    /// Whether `path` is the project file this server serves.
    fn is_project_file(&self, path: &Path) -> bool {
        let project_file = self.project().file_location.clone();
        canonicalize_or_original(&self.vfs, path.to_path_buf())
            == canonicalize_or_original(&self.vfs, project_file)
    }

    /// Reads the project file again, so two-way sync writes with its current
    /// rules. When the syncback rules changed, instances whose layout is stale
    /// move to the one the rules ask for if Studio is authoritative. Any rule
    /// counts, since lifting an exclusion can let a refused relayout go ahead.
    /// Under Filesystem authority the files on disk decide, so layouts stay as
    /// they are. Before any write says which side wins, the first one decides.
    fn reload_project(&self) {
        let current = self.project();
        // A project that stops naming itself keeps the name it was served with,
        // the way the server named it when it started.
        let reloaded = match Project::load_exact(
            &self.vfs,
            &current.file_location,
            current.name.as_deref(),
        ) {
            Ok(project) => project,
            Err(error) => {
                log::warn!(
                    "The project file could not be read again, so its old rules are kept ({error:#})."
                );
                return;
            }
        };
        if current.syncback_rules == reloaded.syncback_rules {
            *self.project.project.lock().unwrap() = Arc::new(reloaded);
            return;
        }
        *self.project.project.lock().unwrap() = Arc::new(reloaded);
        let authority = *self.project.authority.lock().unwrap();
        match authority {
            Some(WriteAuthority::Studio) => self.relayout(),
            Some(WriteAuthority::Filesystem) => log::debug!(
                "The syncback rules changed, but the filesystem is authoritative, so layouts on disk stay as they are."
            ),
            None => self
                .project
                .relayout_pending
                .store(true, std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Remembers which side a write says wins, and applies `modelTrees` rules
    /// that changed before any write said so if it's Studio.
    fn note_authority(&self, authority: WriteAuthority) {
        *self.project.authority.lock().unwrap() = Some(authority);
        let pending = self
            .project
            .relayout_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if pending && authority == WriteAuthority::Studio {
            self.relayout();
        }
    }

    /// Moves every instance whose layout the `modelTrees` rules no longer ask
    /// for to the one they do, keeping its ID, so Studio sees no change.
    fn relayout(&self) {
        let project = self.project();
        let rules = match CompiledModelTreeRules::new(
            project
                .syncback_rules
                .as_ref()
                .map(|rules| rules.model_trees.as_slice())
                .unwrap_or_default(),
            &project.tree,
        ) {
            Ok(rules) => rules,
            Err(error) => {
                log::warn!("The modelTrees rules are invalid ({error:#}).");
                return;
            }
        };
        let mut tree = self.tree.lock().unwrap();
        // A model file can't sit in a folder's metadata, so an instance stored
        // there moves with its outermost ancestor in the same file, which is
        // written out as files of its own with every rule applied.
        let mut targets: Vec<Ref> = Vec::new();
        for mut id in stale_layouts(
            &rules,
            &tree,
            crate::syncback::instances_as_directories(&project),
        ) {
            while let Some(parent) = tree.get_instance(id).map(|instance| instance.parent()) {
                if children_stored_in(&tree, parent).is_none() {
                    break;
                }
                id = parent;
            }
            if !targets.contains(&id) {
                targets.push(id);
            }
        }
        let mut moved = 0;
        for id in targets {
            let Some(instance) = tree.get_instance(id) else {
                continue;
            };
            let name = instance.name().to_owned();
            match self.relayout_instance(&mut tree, id) {
                Ok(()) => moved += 1,
                Err(error) => {
                    log::warn!("Could not move \"{name}\" to its new layout ({error:#}).")
                }
            }
        }
        if moved > 0 {
            log::info!(
                "Moved {moved} {} to the layout the modelTrees rules ask for.",
                if moved == 1 { "instance" } else { "instances" }
            );
        }
    }

    /// Rewrites one instance and its descendants in the layout the project's
    /// rules give it, under the same IDs. Its old files are set aside first, so
    /// the new ones can take their name. If writing fails, or the new files
    /// would lose anything, the old files and the tree are put back.
    fn relayout_instance(&self, tree: &mut RojoTree, id: Ref) -> anyhow::Result<()> {
        if let Some(reason) = package_write_block(tree, &self.vfs, id, true, false) {
            anyhow::bail!("{reason}");
        }
        // Another server's startup recovery must not undo this one midway.
        let _guard = crate::syncback::relocation::lock_relayouts(&self.vfs, &self.project())?;
        *self.project.receipts.lock().unwrap() = Some(WriteReceipts::default());
        let result = self.relayout_recorded(tree, id);
        *self.project.receipts.lock().unwrap() = None;
        result
    }

    /// Does the work of [`Self::relayout_instance`] while every installed path
    /// is recorded.
    fn relayout_recorded(&self, tree: &mut RojoTree, id: Ref) -> anyhow::Result<()> {
        let instance = tree.get_instance(id).context("The instance is gone")?;
        let parent = instance.parent();
        anyhow::ensure!(
            children_stored_in(tree, parent).is_none(),
            "its parent keeps its children in a shared file, which can't hold a model file"
        );
        let context = WriteContext {
            cursor: None,
            stale: false,
            authority: WriteAuthority::Studio,
            batch: &[],
        };
        let backup = backup_subtree(tree, id);

        // An instance in its folder's metadata moves out of it like any move.
        if let Some(stored) = stored_in(tree, id) {
            let owner = tree
                .get_instance(stored.owner())
                .context("The file's owner is gone")?;
            let owner_files =
                FsSnapshot::capture_file_contents(&owner.metadata().relevant_paths, &self.vfs)?;
            let result = self.persist_stored_move(
                tree,
                &PatchUpdate {
                    changed_parent: Some(parent),
                    ..empty_update(id)
                },
                &context,
            );
            return self.finish_relayout(tree, id, parent, backup, result, &owner_files, || Ok(()));
        }

        let path = match self.check_removal(tree, id, &context)? {
            Removal::Allowed(path) => path,
            Removal::Restore(_) => anyhow::bail!("its files changed on disk"),
        };
        let mut entries = vec![path.clone()];
        entries.extend(own_sidecars(tree, id, &path));
        // Every file that moves is checked. Files another instance also comes
        // from, like a folder two project nodes both map, would be taken from
        // that instance too, and files the project's syncback rules exclude
        // are never rewritten, like a full syncback leaves them alone.
        let subtree: HashSet<Ref> = tree.descendants(id).map(|instance| instance.id()).collect();
        let project = self.project();
        let globs = match &project.syncback_rules {
            Some(rules) => Some(rules.compile_globs()?),
            None => None,
        };
        let mut pending = entries.clone();
        while let Some(entry) = pending.pop() {
            anyhow::ensure!(
                tree.get_ids_at_path(&entry)
                    .iter()
                    .all(|holder| subtree.contains(holder)),
                "another instance also comes from \"{}\"",
                entry.display()
            );
            anyhow::ensure!(
                crate::syncback::is_valid_path(&globs, project.folder_location(), &entry),
                "\"{}\" is excluded by the project's syncback rules",
                entry.display()
            );
            if self
                .vfs
                .metadata(&entry)
                .with_not_found()?
                .is_some_and(|metadata| metadata.is_dir())
            {
                for child in self.vfs.read_dir(&entry)? {
                    pending.push(child?.path().to_path_buf());
                }
            }
        }
        let name = instance.name().to_owned();
        let (model, ids) = subtree_model(tree, id, &name);

        // Kept apart from the staging that recovery clears. The manifest is
        // written before anything moves, so startup recovery can put the files
        // back if the server stops partway.
        let aside = crate::syncback::relocation::relayout_directory(&project)
            .join(uuid::Uuid::new_v4().to_string());
        self.vfs.create_dir_all(&aside)?;
        let mut existing = Vec::new();
        for entry in &entries {
            if self.vfs.metadata(entry).with_not_found()?.is_some() {
                existing.push(entry.clone());
            }
        }
        let manifest = crate::syncback::relocation::RelayoutManifest::prepare(
            &self.vfs,
            &aside,
            &existing,
            &self.children_directory(tree, parent)?,
        )?;
        // Published whole and flushed, with the folders that lead to it, before
        // any source moves, so a power loss can't leave backups without it.
        self.vfs.lock().write_atomic(
            &aside.join(crate::syncback::relocation::RELAYOUT_MANIFEST_NAME),
            &serde_json::to_vec(&manifest)?,
            None,
        )?;
        let mut flushed = Some(aside.as_path());
        while let Some(folder) = flushed {
            self.vfs.sync_path(folder)?;
            if folder == project.folder_location() {
                break;
            }
            flushed = folder.parent();
        }
        let mut moved = Vec::new();
        let set_aside = (|| -> io::Result<()> {
            manifest.validate_package(&self.vfs, &aside)?;
            for (index, entry) in existing.iter().enumerate() {
                let destination = aside.join(index.to_string());
                let expected = match manifest.expected_original(index) {
                    Some(expected) => expected.clone(),
                    None => self.vfs.snapshot_tree(entry)?,
                };
                self.vfs.move_no_replace(entry, &destination, &expected)?;
                moved.push((entry.clone(), destination));
            }
            Ok(())
        })();
        let put_back = |moved: &[(PathBuf, PathBuf)]| {
            if let Err(error) = manifest.validate_package(&self.vfs, &aside) {
                log::error!("Relayout recovery refused ({error}). Backups retained.");
                return;
            }
            for (original, destination) in moved {
                let restored = self.vfs.snapshot_tree(destination).and_then(|expected| {
                    self.vfs.move_no_replace(destination, original, &expected)
                });
                if let Err(error) = restored {
                    log::error!(
                        "Could not put \"{}\" back from \"{}\" ({error}).",
                        original.display(),
                        destination.display()
                    );
                    return;
                }
            }
            let _ = self.vfs.remove_dir_all(&aside);
        };
        if let Err(error) = set_aside {
            put_back(&moved);
            return Err(error.into());
        }
        manifest.validate_package(&self.vfs, &aside)?;
        let result = self.place_subtree(tree, parent, model, &ids, &context, &[id]);
        manifest.validate_package(&self.vfs, &aside)?;
        let lossless = matches!(&result, Ok(errors) if errors.is_empty());
        self.finish_relayout(tree, id, parent, backup, result, &HashMap::new(), || {
            put_back(&moved);
            Ok(())
        })?;
        if lossless {
            self.vfs.remove_dir_all(&aside)?;
        }
        Ok(())
    }

    /// Keeps a relayout that saved everything. Otherwise deletes the paths it
    /// installed, puts each file it rewrote back to its contents in `before`,
    /// calls `restore_files` to put set-aside files back, and puts the subtree
    /// back in the tree under its IDs. A rewritten file is only put back while
    /// it still holds what the relayout wrote, so an editor's save is kept.
    #[allow(clippy::too_many_arguments)]
    fn finish_relayout(
        &self,
        tree: &mut RojoTree,
        id: Ref,
        parent: Ref,
        backup: (InstanceSnapshot, SnapshotReferents),
        result: anyhow::Result<Vec<WriteError>>,
        before: &ExpectedFiles,
        restore_files: impl FnOnce() -> io::Result<()>,
    ) -> anyhow::Result<()> {
        let error = match result {
            Ok(errors) if errors.is_empty() => return Ok(()),
            Ok(errors) => anyhow::anyhow!(
                "the new layout couldn't keep everything ({})",
                errors
                    .iter()
                    .map(|error| error.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Err(error) => error,
        };
        // Exactly what was written is undone, which the tree may not know
        // about yet if reading the new files back is what failed.
        let receipts = self
            .project
            .receipts
            .lock()
            .unwrap()
            .replace(WriteReceipts::default())
            .unwrap_or_default();
        {
            let mut lock = self.vfs.lock();
            for path in receipts.installed {
                match lock.metadata(&path).with_not_found()? {
                    Some(metadata) if metadata.is_dir() => lock.remove_dir_all(&path)?,
                    Some(_) => lock.remove_file(&path)?,
                    None => {}
                }
            }
            for (path, written) in receipts.rewritten {
                let Some(before) = before.get(&path) else {
                    continue;
                };
                let current = lock
                    .read(&path)
                    .with_not_found()?
                    .map(|bytes| (*bytes).clone());
                if current != written {
                    log::warn!(
                        "Kept \"{}\", which changed after the relayout wrote it.",
                        path.display()
                    );
                    continue;
                }
                match before {
                    Some(before) => lock.write_atomic(&path, before, current.as_deref())?,
                    None => {
                        lock.remove_file(&path).with_not_found()?;
                    }
                }
            }
        }
        if tree.get_instance(id).is_some() {
            tree.remove(id);
        }
        restore_files()?;
        let (snapshot, referents) = backup;
        tree.insert_instance_with_referents(parent, snapshot, &referents, &mut Vec::new());
        Err(error)
    }

    /// Deletes the empty `.gitkeep`s syncback left in `directory`, which just
    /// gained contents, and in the directories holding it, which aren't empty
    /// either. Only directories inside the project folder are looked at, one
    /// check each, and a `.gitkeep` with contents was written by someone else
    /// and is kept.
    fn remove_stale_keep_files(&self, directory: &Path) {
        let root =
            canonicalize_or_original(&self.vfs, self.project().folder_location().to_path_buf());
        let start = canonicalize_or_original(&self.vfs, directory.to_path_buf());
        let mut directory = Some(start.as_path());
        while let Some(current) = directory {
            if !current.starts_with(&root) {
                break;
            }
            let keep = current.join(EMPTY_DIR_KEEP_NAME);
            // The event can be late, so the folder must still hold something
            // other than its marker, or the marker is still needed.
            let has_contents = self.vfs.read_dir(current).is_ok_and(|mut entries| {
                entries.any(|entry| entry.is_ok_and(|entry| !is_keep_file(entry.path())))
            });
            if !has_contents {
                break;
            }
            match self.vfs.read(&keep).with_not_found() {
                Ok(Some(contents)) if contents.is_empty() => {
                    if let Err(error) = self.vfs.remove_file(&keep).with_not_found() {
                        log::warn!("Could not remove \"{}\" ({error}).", keep.display());
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("Could not read \"{}\" ({error}).", keep.display()),
            }
            directory = current.parent();
        }
    }

    /// Keeps a directory in version control once a deletion from Studio left
    /// it empty, the way syncback keeps an empty directory.
    fn keep_if_empty(&self, directory: &Path) {
        let Ok(Some(mut entries)) = self.vfs.read_dir(directory).with_not_found() else {
            return;
        };
        if entries.next().is_none() {
            let keep = directory.join(EMPTY_DIR_KEEP_NAME);
            if let Err(error) = self.vfs.write(&keep, []) {
                log::warn!("Could not write \"{}\" ({error}).", keep.display());
            }
        }
    }

    /// Returns whether an event is about a path inside the project's `.rojo`
    /// state directory. Watchers report canonical paths, so both spellings of
    /// the project folder are checked.
    fn is_in_state_directory(&self, event: &VfsEvent) -> bool {
        let (VfsEvent::Create(path) | VfsEvent::Write(path) | VfsEvent::Remove(path)) = event
        else {
            return false;
        };
        let project = self.project();
        let folder = project.folder_location();
        let canonical = self.vfs.canonicalize(folder).ok();
        [Some(folder.to_path_buf()), canonical]
            .into_iter()
            .flatten()
            .any(|folder| path.starts_with(folder.join(".rojo")))
    }

    #[cfg(test)]
    fn handle_tree_event(&self, patch_set: PatchSet) -> WriteResult {
        self.handle_tree_event_at_cursor(
            patch_set,
            Some(self.message_queue.cursor()),
            WriteAuthority::Filesystem,
        )
    }

    #[cfg(test)]
    fn handle_tree_event_at_cursor(
        &self,
        patch_set: PatchSet,
        message_cursor: Option<u32>,
        authority: WriteAuthority,
    ) -> WriteResult {
        self.handle_write(patch_set, &HashMap::new(), message_cursor, authority)
    }

    #[profiling::function]
    fn handle_write(
        &self,
        patch_set: PatchSet,
        additions: &HashMap<Ref, InstanceAddition>,
        message_cursor: Option<u32>,
        authority: WriteAuthority,
    ) -> WriteResult {
        // Reject ownership transfers against the original tree, before authority
        // selection, additions, or cycle recovery can write files or revert Studio.
        let ownership_blocks: HashMap<_, _> = {
            let tree = self.tree.lock().unwrap();
            patch_set
                .updated_instances
                .iter()
                .filter_map(|update| {
                    let mut parent = update.changed_parent?;
                    let mut visited = HashSet::new();
                    while tree.get_instance(parent).is_none() && visited.insert(parent) {
                        let Some(addition) = additions.get(&parent) else {
                            break;
                        };
                        parent = addition.parent;
                    }
                    package_reparent_block(&tree, update.id, parent)
                        .map(|reason| (update.id, reason))
                })
                .collect()
        };
        let only_protected = {
            let tree = self.tree.lock().unwrap();
            let mut targets = Vec::new();
            targets.extend(patch_set.updated_instances.iter().map(|update| {
                ownership_blocks.get(&update.id).cloned().or_else(|| {
                    package_write_block(
                        &tree,
                        &self.vfs,
                        update.id,
                        update.changed_parent.is_some()
                            || update.changed_name.is_some()
                            || update.changed_class_name.is_some(),
                        false,
                    )
                    .or_else(|| {
                        update.changed_parent.and_then(|parent| {
                            package_write_block(&tree, &self.vfs, parent, false, false)
                        })
                    })
                })
            }));
            targets.extend(
                patch_set
                    .removed_instances
                    .iter()
                    .map(|id| package_write_block(&tree, &self.vfs, *id, true, false)),
            );
            targets.extend(additions.values().map(|addition| {
                package_write_block(&tree, &self.vfs, addition.parent, false, false)
            }));
            !targets.is_empty() && targets.iter().all(Option::is_some)
        };
        // A warning-only package request must not activate pending filesystem
        // relayouts as an unrelated side effect of selecting Studio authority.
        if !only_protected {
            self.note_authority(authority);
        }
        // A request is based on the last revision applied before its local batch began.
        // The current server DOM is not a baseline when a watcher has advanced it.
        log::trace!("Persisting PatchSet from client: {:#?}", patch_set);
        let mut accepted = PatchSet::new();
        let mut conflicts = PatchSet::new();
        let mut errors = Vec::new();
        let mut messages = Vec::new();
        let mut deletion_conflicts = Vec::new();
        let mut with_ancestors = Vec::new();
        let mut tree = self.tree.lock().unwrap();
        let context = WriteContext {
            cursor: message_cursor,
            stale: message_cursor != Some(self.message_queue.cursor()),
            authority,
            batch: &patch_set.updated_instances,
        };
        {
            let package_block = |tree: &RojoTree, id: Ref, structural: bool| {
                package_write_block(tree, &self.vfs, id, structural, context.stale)
            };
            // Created subtrees go first, so the rest of the batch can move
            // existing instances into them.
            for (&id, addition) in additions {
                if let Some(reason) = package_block(&tree, addition.parent, false) {
                    log::warn!("Skipping package addition. {reason}. Studio was left unchanged");
                    errors.push(WriteError {
                        id,
                        property: None,
                        message: reason,
                    });
                    continue;
                }
                match self.persist_addition(&mut tree, id, addition, &context, &mut messages) {
                    Ok(unsaved) => errors.extend(unsaved),
                    Err(err) => errors.push(WriteError {
                        id,
                        property: None,
                        message: format!("Could not create this instance on disk: {err:#}"),
                    }),
                }
            }
            let mut updates: Vec<_> = patch_set
                .updated_instances
                .iter()
                .filter_map(|update| {
                    if let Some(reason) = ownership_blocks.get(&update.id) {
                        log::warn!(
                            "Skipping package update for {}. {reason}. Studio was left unchanged",
                            update.id
                        );
                        errors.push(WriteError {
                            id: update.id,
                            property: Some("Parent".into()),
                            message: reason.clone(),
                        });
                        None
                    } else {
                        Some(update.clone())
                    }
                })
                .collect();
            let order = crate::syncback::relocation::relocation_order(
                &tree,
                &updates,
                &patch_set.removed_instances,
            );
            let order = match order {
                Ok(order) => order,
                Err(error) => {
                    // Invalid structural edits must not discard independent property edits.
                    for update in &mut updates {
                        if update.changed_parent.take().is_some() {
                            errors.push(WriteError {
                                id: update.id,
                                property: Some("Parent".into()),
                                message: format!("{error:#}"),
                            });
                            if let Some(instance) = tree.get_instance(update.id) {
                                conflicts.updated_instances.push(PatchUpdate {
                                    id: update.id,
                                    changed_parent: Some(instance.parent()),
                                    ..empty_update(update.id)
                                });
                            }
                        }
                    }
                    (0..updates.len()).collect()
                }
            };
            // Package property edits form one durable batch. Reuse full project
            // serialization so transparent/nested project entrypoints route exactly
            // as one-shot syncback does, instead of treating them as project-owned.
            let mut package_updates = HashSet::new();
            let mut local_updates = Vec::new();
            for update in &updates {
                let mut current = Some(update.id);
                let mut local = false;
                while let Some(instance) = current.and_then(|id| tree.get_instance(id)) {
                    if instance.metadata().context.package_mount.is_some() {
                        local = true;
                        break;
                    }
                    if instance.metadata().instigating_source.is_some() {
                        break;
                    }
                    current = Some(instance.parent());
                }
                if !local
                    || update.changed_name.is_some()
                    || update.changed_parent.is_some()
                    || update.changed_class_name.is_some()
                    || update.changed_metadata.is_some()
                {
                    continue;
                }
                package_updates.insert(update.id);
                if let Some(reason) = package_block(&tree, update.id, false) {
                    log::warn!("Skipping package update. {reason}. Studio was left unchanged");
                    errors.push(WriteError {
                        id: update.id,
                        property: None,
                        message: reason,
                    });
                } else {
                    let project = self.project();
                    let instance = tree.get_instance(update.id).unwrap();
                    let filter_tree = WeakDom::new(InstanceBuilder::new(instance.class_name()));
                    let ignored =
                        get_property_filter(&project, filter_tree.root()).unwrap_or_default();
                    let path = tree.inner().full_path_of(update.id, "/");
                    let ignored_tree = project.syncback_rules.as_ref().is_some_and(|rules| {
                        rules
                            .ignore_trees
                            .iter()
                            .any(|ignored| path.starts_with(ignored))
                    });
                    let mut allowed = update.clone();
                    allowed.changed_properties.retain(|property, _| {
                        let reason = if ignored_tree {
                            "This instance is excluded by the project's syncback tree rules"
                        } else if ignored.contains(property) {
                            "This property is excluded by the project's syncback rules"
                        } else if !can_write_property(
                            &project,
                            instance.class_name().as_str(),
                            property.as_str(),
                        ) {
                            "This property is excluded by reflection serialization rules"
                        } else {
                            return true;
                        };
                        errors.push(WriteError {
                            id: update.id,
                            property: Some(property.to_string()),
                            message: reason.into(),
                        });
                        false
                    });
                    if !allowed.changed_properties.is_empty() {
                        local_updates.push(allowed);
                    }
                }
            }
            if !local_updates.is_empty() {
                let result = (|| -> anyhow::Result<()> {
                    let builder = |id: Ref| {
                        let instance = tree.get_instance(id).unwrap();
                        InstanceBuilder::new(instance.class_name())
                            .with_name(instance.name())
                            .with_properties(instance.properties().clone())
                            .with_referent(id)
                    };
                    let root = tree.get_root_id();
                    let mut edited = WeakDom::new(builder(root));
                    let mut pending = vec![root];
                    while let Some(parent) = pending.pop() {
                        for &child in tree.get_instance(parent).unwrap().children() {
                            edited.insert(parent, builder(child));
                            pending.push(child);
                        }
                    }
                    for update in &local_updates {
                        let instance = edited
                            .get_by_ref_mut(update.id)
                            .context("Package instance disappeared")?;
                        for (property, value) in &update.changed_properties {
                            if let Some(value) = value {
                                instance
                                    .properties
                                    .insert(*property, normalize_json_variant(value.clone()));
                            } else {
                                instance.properties.remove(property);
                            }
                        }
                    }
                    let project = self.project();
                    let mut baseline = tree.clone_for_serialization();
                    let files =
                        crate::syncback::syncback_loop(&self.vfs, &mut baseline, edited, &project)?;
                    files.write_to_vfs("", &self.vfs)?;
                    let mut owners = HashSet::new();
                    let mut projects = HashMap::new();
                    for update in &local_updates {
                        let mut owner = update.id;
                        while let Some(instance) = tree.get_instance(owner) {
                            match &instance.metadata().instigating_source {
                                Some(InstigatingSource::ProjectNode { path, .. }) => {
                                    // A cached project node predates this write. Refresh
                                    // its containing project, not that cached node.
                                    let project_path = path.clone();
                                    loop {
                                        let parent = tree
                                            .get_instance(owner)
                                            .context("The containing project disappeared")?;
                                        if parent.metadata().relevant_paths.contains(&project_path)
                                        {
                                            break;
                                        }
                                        owner = parent.parent();
                                    }
                                    // Transparent projects share an owner. Re-enter its
                                    // outer project so mount-entrance watches survive.
                                    let entrypoint = tree
                                        .get_instance(owner)
                                        .unwrap()
                                        .metadata()
                                        .relevant_paths
                                        .iter()
                                        .rev()
                                        .find(|path| Project::is_project_file(path))
                                        .cloned()
                                        .unwrap_or(project_path);
                                    projects.insert(owner, entrypoint);
                                    break;
                                }
                                Some(InstigatingSource::Path(_)) => break,
                                None => owner = instance.parent(),
                            }
                        }
                        owners.insert(owner);
                    }
                    // Moving a whole source directory need not produce per-file
                    // watcher events. Refresh the durable result before acknowledging
                    // it, including original/emitted source and expected-byte baselines.
                    for owner in owners {
                        if let Some(path) = projects.get(&owner) {
                            let instance = tree
                                .get_instance(owner)
                                .context("The containing project disappeared")?;
                            let mut context = instance.metadata().context.clone();
                            // Re-enter this project with its ancestor chain, not
                            // its own active-project marker. The normal middleware
                            // also refreshes mount paths and expected file bytes.
                            if let Some(active) = &mut context.active_projects {
                                let canonical = self.vfs.canonicalize(path)?;
                                if let Some(index) =
                                    active.iter().position(|path| path == &canonical)
                                {
                                    Arc::make_mut(active).truncate(index);
                                }
                            }
                            let mut snapshot = snapshot_from_vfs(&context, &self.vfs, path)?
                                .context("The saved project is no longer visible")?;
                            snapshot.name = instance.name().to_owned().into();
                            snapshot.metadata.instigating_source =
                                instance.metadata().instigating_source.clone();
                            let patch = compute_patch_set(Some(snapshot), &tree, owner);
                            messages.push(apply_patch_set(&mut tree, patch));
                        } else if let Some(applied) =
                            compute_and_apply_changes(&mut tree, &self.vfs, owner, false)
                        {
                            messages.push(applied);
                        }
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        let database = rbx_reflection_database::get().unwrap();
                        for mut update in local_updates {
                            let instance = tree.get_instance(update.id);
                            let defaults = instance
                                .and_then(|instance| {
                                    database.classes.get(instance.class_name().as_str())
                                })
                                .map(|class| &class.default_properties);
                            // Successful serialization can still omit a path or an
                            // unsupported property. Acknowledge only the refreshed
                            // durable value, never merely a successful batch.
                            let mut corrections = UstrMap::new();
                            update.changed_properties.retain(|property, requested| {
                                let default = defaults.and_then(|values| values.get(property.as_str()));
                                let saved_property = instance.and_then(|instance| instance.properties().get(property));
                                let saved = saved_property.or(default);
                                let intended = requested.clone().map(normalize_json_variant).map(|value| {
                                    if property == "Source" {
                                        if let Variant::String(source) = value {
                                            return Variant::String(source.replace("\r\n", "\n"));
                                        }
                                    }
                                    value
                                });
                                let source_matches = property == "Source" && instance
                                    .and_then(|instance| instance.metadata().source_transform.as_ref())
                                    .is_some_and(|transform| {
                                        saved == Some(&Variant::String(transform.emitted.clone()))
                                            && intended == Some(Variant::String(transform.original.clone()))
                                    });
                                if instance.is_some() && (source_matches || same_value(saved, intended.as_ref().or(default))) {
                                    // Match the ordinary property writer's exact-value
                                    // correction contract, including omitted defaults.
                                    if requested.is_some() && requested.as_ref() != saved
                                        && serde_json::to_value(&requested).expect("Variants must serialize")
                                            != serde_json::to_value(saved).expect("Variants must serialize")
                                    {
                                        corrections.insert(*property, saved_property.cloned());
                                    }
                                    *requested = saved_property.cloned();
                                    return true;
                                }
                                errors.push(WriteError {
                                    id: update.id,
                                    property: Some(property.to_string()),
                                    message: "The package property was not saved by the project's syncback rules or file format".into(),
                                });
                                false
                            });
                            if !corrections.is_empty() {
                                messages.push(AppliedPatchSet {
                                    updated: vec![AppliedPatchUpdate {
                                        changed_properties: corrections,
                                        ..AppliedPatchUpdate::new(update.id)
                                    }],
                                    ..AppliedPatchSet::new()
                                });
                            }
                            if !update.changed_properties.is_empty() {
                                accepted.updated_instances.push(update);
                            }
                        }
                    }
                    Err(error) => {
                        log::warn!("Package source batch was not written. {error:#}. Studio was left unchanged");
                        for update in local_updates {
                            errors.push(WriteError {
                                id: update.id,
                                property: None,
                                message: format!("Package source batch was not written. {error:#}"),
                            });
                        }
                    }
                }
            }
            // Retained descendants must reach their destinations before an old
            // ancestor's filesystem tree can be removed.
            for index in order {
                if package_updates.contains(&updates[index].id) {
                    continue;
                }
                let update = &updates[index];
                let structural = update.changed_parent.is_some()
                    || update.changed_name.is_some()
                    || update.changed_class_name.is_some();
                let reason = package_block(&tree, update.id, structural).or_else(|| {
                    update
                        .changed_parent
                        .and_then(|parent| package_block(&tree, parent, false))
                });
                if let Some(reason) = reason {
                    log::warn!(
                        "Skipping package update for {}. {reason}. Studio was left unchanged",
                        update.id
                    );
                    errors.push(WriteError {
                        id: update.id,
                        property: None,
                        message: reason,
                    });
                    continue;
                }
                self.persist_update(
                    &mut tree,
                    &updates[index],
                    &mut WriteOutcome {
                        accepted: &mut accepted,
                        conflicts: &mut conflicts,
                        errors: &mut errors,
                        messages: &mut messages,
                    },
                    &context,
                    1,
                );
            }
            let removed: HashSet<Ref> = patch_set.removed_instances.iter().copied().collect();
            for &id in &patch_set.removed_instances {
                if let Some(reason) = package_block(&tree, id, true) {
                    log::warn!(
                        "Skipping package removal for {id}. {reason}. Studio was left unchanged"
                    );
                    errors.push(WriteError {
                        id,
                        property: None,
                        message: reason,
                    });
                    continue;
                }
                // Deleting a descendant first, then its ancestor, arrives as one
                // batch. The ancestor's removal takes the descendant with it, and
                // removing the descendant first would make the ancestor look
                // changed on disk.
                let mut ancestor = tree.get_instance(id).map(|instance| instance.parent());
                let mut removed_ancestor = None;
                while let Some(parent) = ancestor.and_then(|parent| tree.get_instance(parent)) {
                    if removed.contains(&parent.id()) {
                        removed_ancestor = Some(parent.id());
                        break;
                    }
                    ancestor = Some(parent.parent());
                }
                if let Some(ancestor) = removed_ancestor {
                    with_ancestors.push((id, ancestor));
                    continue;
                }
                let retained_move_failed = tree.get_instance(id).is_some()
                    && patch_set.updated_instances.iter().any(|update| {
                        update.changed_parent.is_some()
                            && tree
                                .descendants(id)
                                .any(|instance| instance.id() == update.id)
                    });
                if retained_move_failed {
                    if let Some(instance) = tree.get_instance(id) {
                        deletion_conflicts.push(PatchAdd {
                            parent_id: instance.parent(),
                            instance: snapshot_from_tree(&tree, id),
                        });
                        messages.push(AppliedPatchSet {
                            removed: vec![id],
                            added: vec![id],
                            updated: Vec::new(),
                        });
                    }
                    errors.push(WriteError {
                        id,
                        property: None,
                        message: "Cannot remove an ancestor while a retained descendant's move is unapplied".into(),
                    });
                    continue;
                }
                // An instance inside a model file is removed by rewriting the
                // model without it.
                if let Some(StoredIn::Model(model)) = stored_in(&tree, id) {
                    let stored = StoredIn::Model(model);
                    // Like any other write, a disk change Studio hasn't seen wins
                    // under Filesystem authority and is caught up with first under
                    // Studio authority. Returns whether the instance is gone.
                    let result = (|| -> anyhow::Result<bool> {
                        if !self.catch_up_stored(
                            &mut tree,
                            stored,
                            &context,
                            &mut messages,
                            None,
                        )? {
                            return Ok(false);
                        }
                        if tree.get_instance(id).is_none() {
                            return Ok(true);
                        }
                        let mut copy = StoredCopy::new(&tree, stored);
                        let removed = copy
                            .copy_of(id)
                            .context("The instance isn't stored in this model")?;
                        copy.delete(removed);
                        let written = self.write_stored(&tree, stored, copy)?;
                        errors.extend(Self::apply_stored(&mut tree, written, &[]));
                        Ok(true)
                    })();
                    let restore = match result {
                        // Rewriting the model already took the instance out of the
                        // tree, so the removal is only reported.
                        Ok(true) => {
                            accepted.removed_instances.push(id);
                            messages.push(AppliedPatchSet {
                                removed: vec![id],
                                ..AppliedPatchSet::new()
                            });
                            false
                        }
                        Ok(false) => true,
                        Err(err) => {
                            errors.push(WriteError {
                                id,
                                property: None,
                                message: format!("{err:#}"),
                            });
                            true
                        }
                    };
                    if restore {
                        if let Some(instance) = tree.get_instance(id) {
                            deletion_conflicts.push(PatchAdd {
                                parent_id: instance.parent(),
                                instance: snapshot_from_tree(&tree, id),
                            });
                            messages.push(AppliedPatchSet {
                                removed: vec![id],
                                added: vec![id],
                                updated: Vec::new(),
                            });
                        }
                    }
                    continue;
                }
                if let Some(owner) = inline_owner(&tree, id) {
                    let removal =
                        self.remove_inline_instance(&mut tree, id, owner, &context, &mut messages);
                    let latest = match removal {
                        Ok(InlineRemoval::Removed) => {
                            accepted.removed_instances.push(id);
                            continue;
                        }
                        Ok(InlineRemoval::Restore(latest)) => *latest,
                        Err(err) => {
                            errors.push(WriteError {
                                id,
                                property: None,
                                message: format!("{err:#}"),
                            });
                            match self.snapshot_owner(&tree, owner) {
                                Ok(latest) => latest,
                                Err(error) => {
                                    errors.push(WriteError {
                                        id,
                                        property: None,
                                        message: format!(
                                            "Could not restore filesystem state: {error:#}"
                                        ),
                                    });
                                    continue;
                                }
                            }
                        }
                    };
                    // The whole owner file is the unit of comparison, so bring every
                    // instance it stores back in line with the disk before re-sending
                    // the one Studio removed.
                    let disk_patch = compute_shallow_patch_set(latest, &tree, owner);
                    let applied = apply_patch_set(&mut tree, disk_patch);
                    if !applied.is_empty() {
                        messages.push(applied);
                    }
                    if let Some(instance) = tree.get_instance(id) {
                        deletion_conflicts.push(PatchAdd {
                            parent_id: instance.parent(),
                            instance: snapshot_from_tree(&tree, id),
                        });
                        messages.push(AppliedPatchSet {
                            removed: vec![id],
                            added: vec![id],
                            updated: Vec::new(),
                        });
                    }
                    continue;
                }
                // The folder that holds the parent's children, including a
                // project-defined one, gets a `.gitkeep` back if this leaves it
                // empty.
                let holder = tree
                    .get_instance(id)
                    .and_then(|instance| self.children_directory(&tree, instance.parent()).ok());
                match self.remove_instance(&tree, id, &context) {
                    Ok(Some(snapshot)) => {
                        if let Some(instance) = tree.get_instance(id) {
                            deletion_conflicts.push(PatchAdd {
                                parent_id: instance.parent(),
                                instance: snapshot.clone(),
                            });
                            let disk_patch = compute_patch_set(Some(snapshot), &tree, id);
                            apply_patch_set(&mut tree, disk_patch);
                            // Studio already removed this subtree. Re-send it without replacing
                            // the server's instances, preserving IDs and reference targets.
                            messages.push(AppliedPatchSet {
                                removed: vec![id],
                                added: vec![id],
                                updated: Vec::new(),
                            });
                        }
                    }
                    Ok(None) => {
                        accepted.removed_instances.push(id);
                        if let Some(holder) = holder {
                            self.keep_if_empty(&holder);
                        }
                    }
                    Err(err) => {
                        errors.push(WriteError {
                            id,
                            property: None,
                            message: format!("{err:#}"),
                        });
                        if let Some(instance) = tree.get_instance(id) {
                            if let Some(InstigatingSource::Path(path)) =
                                instance.metadata().instigating_source.as_ref()
                            {
                                if let Ok(Some(snapshot)) =
                                    snapshot_from_vfs(&instance.metadata().context, &self.vfs, path)
                                {
                                    deletion_conflicts.push(PatchAdd {
                                        parent_id: instance.parent(),
                                        instance: snapshot.clone(),
                                    });
                                    let disk_patch = compute_patch_set(Some(snapshot), &tree, id);
                                    apply_patch_set(&mut tree, disk_patch);
                                    messages.push(AppliedPatchSet {
                                        removed: vec![id],
                                        added: vec![id],
                                        updated: Vec::new(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        // A descendant is deleted when the ancestor it went with was, however
        // deeply its own ancestors were nested in the batch. Otherwise the
        // ancestor's restoration already brought it back.
        let mut pending: Vec<(Ref, Ref)> = with_ancestors;
        loop {
            let before = pending.len();
            pending.retain(|&(id, ancestor)| {
                if accepted.removed_instances.contains(&ancestor) {
                    accepted.removed_instances.push(id);
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                break;
            }
        }
        // Saved updates were applied as they were written, so a later update in
        // the batch sees them. Removals wait until here, since a removed parent
        // must still be known while its removed children are handled.
        if !accepted.removed_instances.is_empty() {
            // Removing an ancestor also removes its descendants, so only what's
            // still in the tree is removed here.
            for &id in &accepted.removed_instances {
                if tree.get_instance(id).is_some() {
                    let removal = PatchSet {
                        removed_instances: vec![id],
                        ..PatchSet::new()
                    };
                    messages.push(apply_patch_set(&mut tree, removal));
                }
            }
        }
        if conflicts != PatchSet::new() {
            // Removals go one at a time for the same reasons as above.
            let mut restorations = conflicts.clone();
            for id in std::mem::take(&mut restorations.removed_instances) {
                if tree.get_instance(id).is_some() {
                    let removal = PatchSet {
                        removed_instances: vec![id],
                        ..PatchSet::new()
                    };
                    messages.push(apply_patch_set(&mut tree, removal));
                }
            }
            messages.push(apply_patch_set(&mut tree, restorations));
        }
        conflicts.added_instances.extend(deletion_conflicts);
        self.message_queue.push_messages(&messages);
        (accepted, conflicts, errors)
    }

    /// Writes a subtree created in Studio as new files and adds it to the tree
    /// under the ids Studio gave it, so announcing it to Studio doesn't create a
    /// second copy. Returns errors for what wasn't saved. Studio stops
    /// tracking a created instance with an error that names no property.
    fn persist_addition(
        &self,
        tree: &mut RojoTree,
        root_id: Ref,
        addition: &InstanceAddition,
        context: &WriteContext,
        messages: &mut Vec<AppliedPatchSet>,
    ) -> anyhow::Result<Vec<WriteError>> {
        anyhow::ensure!(
            addition.ids.id == root_id,
            "The created subtree's root id doesn't match its key"
        );
        let mut ids = Vec::new();
        collect_added_ids(&addition.ids, &mut ids);
        let unique: HashSet<_> = ids.iter().collect();
        anyhow::ensure!(
            unique.len() == ids.len()
                && ids
                    .iter()
                    .all(|id| id.is_some() && tree.get_instance(*id).is_none()),
            "Studio's ids for the created instances are not new and unique"
        );
        anyhow::ensure!(
            tree.get_instance(addition.parent).is_some(),
            "The parent is no longer known"
        );
        let mut model = rbx_binary::from_reader(addition.model.as_slice())
            .context("Could not read the created instances")?;
        // Moved instances keep what their files hold, but new ones leave out
        // what the project's syncback rules ignore.
        drop_ignored_properties(&self.project(), &mut model);
        let unsaved =
            self.place_subtree(tree, addition.parent, model, &addition.ids, context, &[])?;
        if tree.get_instance(root_id).is_some() {
            messages.push(AppliedPatchSet {
                added: vec![root_id],
                ..AppliedPatchSet::new()
            });
        }
        Ok(unsaved)
    }

    /// Rewrites the file `stored` names from `copy`, then reads it back. The
    /// tree isn't changed, so nothing is lost if a later step fails. Pass the
    /// result to [`Self::apply_stored`].
    fn write_stored(
        &self,
        tree: &RojoTree,
        stored: StoredIn,
        copy: StoredCopy,
    ) -> anyhow::Result<WrittenStored> {
        let expected = self.check_stored(tree, stored)?;
        self.write_stored_checked(tree, stored, copy, expected)
    }

    /// Captures the files `stored` names, then makes sure they still hold
    /// what the tree has. Writing against the capture fails if an editor saves
    /// in between, instead of overwriting the save.
    fn check_stored(&self, tree: &RojoTree, stored: StoredIn) -> anyhow::Result<ExpectedFiles> {
        let instance = tree
            .get_instance(stored.owner())
            .context("The file's owner is no longer known")?;
        let expected =
            FsSnapshot::capture_file_contents(&instance.metadata().relevant_paths, &self.vfs)?;
        anyhow::ensure!(
            self.stored_matches_disk(tree, stored)?,
            "The file changed on disk; the change was not saved"
        );
        Ok(expected)
    }

    /// Like [`Self::write_stored`], for a caller that checked the file with
    /// [`Self::check_stored`] before changing the tree.
    #[profiling::function]
    fn write_stored_checked(
        &self,
        tree: &RojoTree,
        stored: StoredIn,
        mut copy: StoredCopy,
        expected: ExpectedFiles,
    ) -> anyhow::Result<WrittenStored> {
        let owner = stored.owner();
        let instance = tree
            .get_instance(owner)
            .context("The file's owner is no longer known")?;
        let Some(InstigatingSource::Path(source)) = instance.metadata().instigating_source.as_ref()
        else {
            anyhow::bail!("The file's owner has no filesystem source");
        };
        let source = source.clone();
        let context = instance.metadata().context.clone();
        let ids = copy.added_ids(copy.dom.root_ref());
        link_outside_refs(tree, &mut copy.dom, &copy.ids);
        let outside = outside_refs(&copy.dom, &copy.ids);
        let staged = match stored {
            StoredIn::Inline(_) => {
                syncback_instance(&self.vfs, tree, &self.project(), owner, copy.dom)?
            }
            StoredIn::Model(_) => {
                syncback_model_file(&self.vfs, tree, &self.project(), owner, copy.dom)?
            }
        };
        validate_write_targets(&self.project(), &staged)?;
        let project = self.project();
        let base = project.folder_location();
        match stored {
            StoredIn::Inline(_) => staged.write_single_file_to_vfs(base, &self.vfs, &expected)?,
            // A model file's adjacent metadata can change along with it.
            StoredIn::Model(_) => {
                staged.write_model_file_to_vfs(base, &self.vfs, &expected, &source)?
            }
        }
        // Recorded as soon as the file is written, since reading it back can
        // still fail, and undoing a relayout needs exactly what was written.
        if let Some(receipts) = self.project.receipts.lock().unwrap().as_mut() {
            for (path, contents) in staged.file_changes() {
                receipts
                    .rewritten
                    .push((base.join(path), contents.map(<[u8]>::to_vec)));
            }
        }
        let snapshot = match stored {
            StoredIn::Inline(_) => snapshot_shallow_from_vfs(&context, &self.vfs, &source)?,
            StoredIn::Model(_) => snapshot_from_vfs(&context, &self.vfs, &source)?,
        }
        .context("The rewritten file is no longer visible")?;
        Ok(WrittenStored {
            stored,
            snapshot,
            ids,
            outside,
        })
    }

    /// Returns whether the file `stored` names still holds what the tree has.
    fn stored_matches_disk(&self, tree: &RojoTree, stored: StoredIn) -> anyhow::Result<bool> {
        let owner = stored.owner();
        let instance = tree.get_instance(owner).unwrap();
        let Some(InstigatingSource::Path(source)) = instance.metadata().instigating_source.as_ref()
        else {
            return Ok(false);
        };
        let context = &instance.metadata().context;
        let patch = match stored {
            StoredIn::Inline(_) => compute_shallow_patch_set(
                snapshot_shallow_from_vfs(context, &self.vfs, source)?,
                tree,
                owner,
            ),
            StoredIn::Model(_) => {
                compute_patch_set(snapshot_from_vfs(context, &self.vfs, source)?, tree, owner)
            }
        };
        Ok(patch == PatchSet::new())
    }

    /// Replaces the instances a rewritten file stores with what was read back,
    /// under the ids the copy recorded. `moving` subtrees are removed first,
    /// since their ids now belong to this file. Returns errors for what the
    /// file couldn't keep.
    fn apply_stored(
        tree: &mut RojoTree,
        written: WrittenStored,
        moving: &[Ref],
    ) -> Vec<WriteError> {
        let owner = written.stored.owner();
        for &id in moving {
            if tree.get_instance(id).is_some() {
                tree.remove(id);
            }
        }
        let stored: Vec<Ref> = tree
            .get_instance(owner)
            .unwrap()
            .children()
            .iter()
            .copied()
            .filter(|&child| stored_in(tree, child) == Some(written.stored))
            .collect();
        for child in stored {
            tree.remove(child);
        }
        let (referents, unstored) = pair_added_ids(&written.snapshot, &written.ids);
        let mut snapshot = written.snapshot;
        if let Some(mut instance) = tree.get_instance_mut(owner) {
            let inner = instance.inner_mut();
            inner.name = snapshot.name.to_string();
            // Kept like insert_instance keeps it, since it's the server's own.
            let pivot = inner.properties.get(&ustr("NeedsPivotMigration")).cloned();
            inner.properties = std::mem::take(&mut snapshot.properties);
            if let Some(pivot) = pivot {
                inner
                    .properties
                    .entry(ustr("NeedsPivotMigration"))
                    .or_insert(pivot);
            }
        }
        // The owner's own references, like a model's PrimaryPart, can point at
        // what it stores too.
        let mut inserted = vec![(snapshot.snapshot_id, owner)];
        let children = std::mem::take(&mut snapshot.children);
        for (child, child_referents) in children.into_iter().zip(referents.children.iter()) {
            tree.insert_instance_with_referents(owner, child, child_referents, &mut inserted);
        }
        tree.resolve_snapshot_refs(&inserted);
        unsaved_errors(unstored, written.outside)
    }

    /// Edits or moves instances whose file is shared: those stored inline in a
    /// directory's metadata, and those inside a model file. Each affected file
    /// is rewritten whole. A move between two files writes the destination
    /// first, so a failure can duplicate the instance but never lose it.
    fn persist_stored_move(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        context: &WriteContext,
    ) -> anyhow::Result<Vec<WriteError>> {
        let id = update.id;
        let parent = update
            .changed_parent
            .context("A move needs a destination")?;
        let instance = tree
            .get_instance(id)
            .context("The moved instance is no longer known")?;
        let mut ancestor = parent;
        while ancestor.is_some() {
            anyhow::ensure!(ancestor != id, "Reparenting would create a cycle");
            ancestor = tree
                .get_instance(ancestor)
                .context("Unknown ancestor")?
                .parent();
        }
        let name = update
            .changed_name
            .clone()
            .unwrap_or_else(|| instance.name().to_owned());
        let source = stored_in(tree, id);
        let destination = children_stored_in(tree, parent);
        let moving = [id];

        // The moved subtree as a model, with the ids it keeps.
        let (model, ids) = subtree_model(tree, id, &name);

        match (source, destination) {
            (Some(from), Some(to)) if from == to => {
                let mut copy = StoredCopy::new(tree, from);
                let moved = copy
                    .copy_of(id)
                    .context("The moved instance isn't stored here")?;
                let target = copy
                    .copy_of(parent)
                    .context("The destination isn't stored here")?;
                copy.dom.transfer_within(moved, target);
                copy.dom.get_by_ref_mut(moved).unwrap().name = name;
                let written = self.write_stored(tree, from, copy)?;
                Ok(Self::apply_stored(tree, written, &[]))
            }
            (_, Some(to)) => {
                // The source's copy is taken before anything changes, without
                // the moved subtree.
                let source_copy = source.map(|from| {
                    let mut copy = StoredCopy::new(tree, from);
                    if let Some(moved) = copy.copy_of(id) {
                        copy.move_out(moved);
                    }
                    (from, copy)
                });
                // Both sides are checked before the destination is written, so
                // a refused move never leaves a copy behind.
                let source_expected = match source {
                    Some(from) => Some(self.check_stored(tree, from)?),
                    None => match self.check_removal(tree, id, context)? {
                        Removal::Allowed(_) => None,
                        Removal::Restore(_) => anyhow::bail!("The moved instance changed on disk"),
                    },
                };
                let mut copy = StoredCopy::new(tree, to);
                copy.add(parent, model, &ids)?;
                let written = self.write_stored(tree, to, copy)?;
                let mut errors = Vec::new();
                match source_copy.zip(source_expected) {
                    Some(((from, copy), expected)) => {
                        let removed = self.write_stored_checked(tree, from, copy, expected)?;
                        errors = Self::apply_stored(tree, removed, &moving);
                    }
                    None => {
                        if self.remove_instance(tree, id, context)?.is_some() {
                            anyhow::bail!("The moved instance changed on disk");
                        }
                    }
                }
                errors.extend(Self::apply_stored(tree, written, &moving));
                Ok(errors)
            }
            (Some(from), None) => {
                // Checked first, since placing the subtree takes it out of the
                // tree while the source file still holds it.
                let expected = self.check_stored(tree, from)?;
                let mut copy = StoredCopy::new(tree, from);
                if let Some(moved) = copy.copy_of(id) {
                    copy.move_out(moved);
                }
                let mut errors = self.place_subtree(tree, parent, model, &ids, context, &moving)?;
                let removed = self.write_stored_checked(tree, from, copy, expected)?;
                errors.extend(Self::apply_stored(tree, removed, &[]));
                Ok(errors)
            }
            (None, None) => anyhow::bail!("Neither side of this move is stored in a shared file"),
        }
    }

    /// Writes a subtree as new files under `parent` and adds it to the tree
    /// under the ids in `ids`. A script parent is promoted to a directory
    /// first, and a parent whose children share its file has that file
    /// rewritten instead. `moving` subtrees are removed from the tree once the
    /// files are in place, since they're what's being written. Returns errors
    /// for what the files couldn't keep.
    fn place_subtree(
        &self,
        tree: &mut RojoTree,
        parent: Ref,
        mut model: WeakDom,
        ids: &AddedIds,
        context: &WriteContext,
        moving: &[Ref],
    ) -> anyhow::Result<Vec<WriteError>> {
        drop_duplicate_ids(&mut model, tree, moving);
        if let Some(stored) = children_stored_in(tree, parent) {
            let mut copy = StoredCopy::new(tree, stored);
            copy.add(parent, model, ids)?;
            let written = self.write_stored(tree, stored, copy)?;
            return Ok(Self::apply_stored(tree, written, moving));
        }

        let parent_instance = tree
            .get_instance(parent)
            .context("The parent is no longer known")?;
        let is_script_file = parent_instance
            .metadata()
            .middleware
            .is_some_and(|middleware| !middleware.is_dir() && middleware != Middleware::Project)
            && crate::syncback::is_script_class(parent_instance.class_name().as_str());
        if is_script_file {
            // Written next to the script, then moved in, which promotes the
            // script to a directory the way a move into it would.
            let holder = parent_instance.parent();
            // Placing the subtree replaces what's moving in the tree, so it's
            // kept to put back if the script can't take it, since the file it
            // came from still holds it.
            let backups: Vec<_> = moving
                .iter()
                .filter_map(|&id| {
                    let instance = tree.get_instance(id)?;
                    Some((instance.parent(), backup_subtree(tree, id)))
                })
                .collect();
            let errors = self.place_subtree(tree, holder, model, ids, context, moving)?;
            let move_in = PatchUpdate {
                changed_parent: Some(parent),
                ..empty_update(ids.id)
            };
            let mut messages = Vec::new();
            let failure = match self.relocate_instance(tree, &move_in, &mut messages, context) {
                Ok(RelocationOutcome::Moved) => return Ok(errors),
                Ok(RelocationOutcome::Restore(_)) => {
                    anyhow::anyhow!("The script changed on disk; the instance was not added")
                }
                Err(err) => err.context("Could not add the instance under the script"),
            };
            let _ = self.remove_instance(tree, ids.id, context);
            if tree.get_instance(ids.id).is_some() {
                tree.remove(ids.id);
            }
            for (parent, (snapshot, referents)) in backups {
                let id = referents.id.unwrap();
                if tree.get_instance(parent).is_some() && tree.get_instance(id).is_none() {
                    let mut inserted = Vec::new();
                    tree.insert_instance_with_referents(
                        parent,
                        snapshot,
                        &referents,
                        &mut inserted,
                    );
                }
            }
            return Err(failure);
        }

        let parent_context = parent_instance.metadata().context.clone();
        let directory = self.children_directory(tree, parent)?;
        let existing: HashSet<String> = self
            .vfs
            .read_dir(&directory)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_lowercase)
            })
            .collect();
        let copies = model_copies(&model, ids);
        link_outside_refs(tree, &mut model, &copies);
        let outside = outside_refs(&model, &copies);
        let new = syncback_new_subtree(
            &self.vfs,
            tree,
            &self.project(),
            parent,
            &directory,
            &existing,
            model,
        )?;
        validate_write_targets(&self.project(), &new.fs_snapshot)?;
        self.install_new_files(&directory, new.fs_snapshot)?;

        let snapshot = snapshot_from_vfs(&parent_context, &self.vfs, &new.root_path)?
            .context("The new instance is not visible on disk")?;
        for &id in moving {
            if tree.get_instance(id).is_some() {
                tree.remove(id);
            }
        }
        let (referents, unstored) = pair_added_ids(&snapshot, ids);
        let mut inserted = Vec::new();
        tree.insert_instance_with_referents(parent, snapshot, &referents, &mut inserted);
        tree.resolve_snapshot_refs(&inserted);
        Ok(unsaved_errors(unstored, outside))
    }

    /// Returns the directory that holds `id`'s children on disk.
    fn children_directory(&self, tree: &RojoTree, id: Ref) -> anyhow::Result<PathBuf> {
        let instance = tree
            .get_instance(id)
            .context("The instance is no longer known")?;
        let metadata = instance.metadata();
        let directory = match metadata.instigating_source.as_ref() {
            Some(InstigatingSource::Path(path))
                if metadata
                    .middleware
                    .is_some_and(|middleware| middleware.is_dir()) =>
            {
                path.clone()
            }
            // A project's root records the project file as its source.
            Some(InstigatingSource::Path(project_file))
                if metadata.middleware == Some(Middleware::Project) =>
            {
                let project = Project::load_exact(&self.vfs, project_file, Some(instance.name()))?;
                project_node_directory(project_file, &project.tree)
                    .context("The project's root has no directory $path")?
            }
            Some(InstigatingSource::ProjectNode { path, node, .. }) => {
                project_node_directory(path, node)
                    .context("The project-defined parent has no directory $path")?
            }
            _ => anyhow::bail!("New instances can only be created inside a directory yet"),
        };
        anyhow::ensure!(
            self.vfs.metadata(&directory)?.is_dir(),
            "The parent's $path is not an existing directory"
        );
        Ok(directory)
    }

    /// Writes new files into a staging directory, then moves them into
    /// `directory` through a recoverable transaction that never replaces an
    /// existing path. A crash leaves either all of them or none of them.
    fn install_new_files(&self, directory: &Path, files: FsSnapshot) -> anyhow::Result<()> {
        use crate::syncback::{
            relocation::{cache_directory, execute_staged_relocation, staging_directory},
            relocation_transaction::Relocation,
        };

        let cache = cache_directory(&self.project());
        self.vfs.create_dir_all(&cache)?;
        self.vfs.sync_path(&cache)?;
        self.vfs.sync_path(cache.parent().unwrap())?;
        let staging = staging_directory(&self.project()).join(uuid::Uuid::new_v4().to_string());
        let files = files.rebase(directory, &staging)?;
        let journal = cache.join(uuid::Uuid::new_v4().to_string());
        let mut destinations = Vec::new();
        let result = execute_staged_relocation(&self.vfs, &journal, || {
            self.vfs.create_dir_all(&staging)?;
            files.write_to_vfs("", &self.vfs)?;
            let mut moves = Vec::new();
            for entry in self.vfs.read_dir(&staging)? {
                let source = entry?.path().to_path_buf();
                let name = source
                    .file_name()
                    .ok_or_else(|| io::Error::other("A staged path has no name"))?;
                let expected = self.vfs.snapshot_tree(&source)?;
                destinations.push(directory.join(name));
                moves.push(Relocation {
                    destination: directory.join(name),
                    source,
                    expected,
                });
            }
            Ok(moves)
        });
        // A failed relocation can leave a journal that still needs its staged
        // files, so those are left for startup recovery, which clears them.
        result?;
        if let Some(receipts) = self.project.receipts.lock().unwrap().as_mut() {
            receipts.installed.extend(destinations);
        }
        self.remove_stale_keep_files(directory);
        // Another server's startup recovery may have cleared it already, once
        // the lock was released.
        self.vfs.remove_dir_all(&staging).with_not_found()?;
        Ok(())
    }

    /// Deletes the files of `id`. Returns what's on disk instead when the disk
    /// must win, which Studio is sent back.
    fn remove_instance(
        &self,
        tree: &RojoTree,
        id: Ref,
        context: &WriteContext,
    ) -> anyhow::Result<Option<crate::snapshot::InstanceSnapshot>> {
        let path = match self.check_removal(tree, id, context)? {
            Removal::Allowed(path) => path,
            Removal::Restore(snapshot) => return Ok(*snapshot),
        };
        let sidecars = own_sidecars(tree, id, &path);
        let mut lock = self.vfs.lock();
        match lock.metadata(&path).with_not_found()? {
            Some(metadata) if metadata.is_file() => {
                lock.remove_file(&path)?;
                for sidecar in sidecars {
                    if lock.metadata(&sidecar).with_not_found()?.is_some() {
                        lock.remove_file(&sidecar)?;
                    }
                }
            }
            Some(_) => lock.remove_dir_all(&path)?,
            None => {}
        }
        Ok(None)
    }

    /// Decides whether the files of `id` can be deleted, without deleting
    /// anything, so a move can check before it writes its destination.
    fn check_removal(
        &self,
        tree: &RojoTree,
        id: Ref,
        context: &WriteContext,
    ) -> anyhow::Result<Removal> {
        let instance = tree
            .get_instance(id)
            .ok_or_else(|| anyhow::anyhow!("Cannot remove unknown instance {id:?}"))?;
        let Some(InstigatingSource::Path(path)) = instance.metadata().instigating_source.as_ref()
        else {
            anyhow::bail!("Cannot remove an instance without a unique filesystem source");
        };
        let snapshot = snapshot_from_vfs(&instance.metadata().context, &self.vfs, path)?;
        let disk_patch = compute_patch_set(snapshot.clone(), tree, id);
        if context.authority == WriteAuthority::Filesystem
            && (context.stale || disk_patch != PatchSet::new())
        {
            return Ok(Removal::Restore(Box::new(snapshot)));
        }
        // A snapshot excludes ignored and unrecognized files. Never delete those as
        // a side effect of removing a represented directory from Studio, whichever
        // side is authoritative, since Studio never had them to begin with.
        if self
            .vfs
            .metadata(path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_dir())
        {
            let mut tracked = HashSet::new();
            tracked.extend(instance.metadata().relevant_paths.iter().cloned());
            for child in tree.descendants(id) {
                tracked.extend(child.metadata().relevant_paths.iter().cloned());
            }
            let mut pending = vec![path.clone()];
            while let Some(directory) = pending.pop() {
                for entry in self.vfs.read_dir(&directory)? {
                    let entry = entry?;
                    // Syncback keeps an empty directory with an empty `.gitkeep`
                    // that no instance lists, so it goes with the directory. One
                    // with contents was written by someone else and is kept.
                    let keep = is_keep_file(entry.path())
                        && self
                            .vfs
                            .read(entry.path())
                            .is_ok_and(|contents| contents.is_empty());
                    if !tracked.contains(entry.path()) && !keep {
                        anyhow::bail!(
                            "The folder also holds \"{}\", which isn't part of the project, \
                             so it was kept",
                            entry.path().display()
                        );
                    }
                    if self.vfs.metadata(entry.path())?.is_dir() {
                        pending.push(entry.path().to_path_buf());
                    }
                }
            }
        }
        Ok(Removal::Allowed(path.clone()))
    }

    /// Reads the current disk state of a directory that stores instances inline.
    fn snapshot_owner(
        &self,
        tree: &RojoTree,
        owner: Ref,
    ) -> anyhow::Result<Option<crate::snapshot::InstanceSnapshot>> {
        let owner = tree
            .get_instance(owner)
            .ok_or_else(|| anyhow::anyhow!("The owning directory is no longer known"))?;
        let Some(InstigatingSource::Path(path)) = owner.metadata().instigating_source.as_ref()
        else {
            anyhow::bail!("The owning directory has no filesystem source");
        };
        snapshot_shallow_from_vfs(&owner.metadata().context, &self.vfs, path)
    }

    /// Removes an instance from its owner's `init.meta.json` by rewriting that
    /// one file, under the same checks as any other Studio write.
    fn remove_inline_instance(
        &self,
        tree: &mut RojoTree,
        id: Ref,
        owner: Ref,
        context: &WriteContext,
        messages: &mut Vec<AppliedPatchSet>,
    ) -> anyhow::Result<InlineRemoval> {
        let authority = context.authority;
        for _ in 0..STUDIO_WRITE_ATTEMPTS {
            let owner_instance = tree
                .get_instance(owner)
                .ok_or_else(|| anyhow::anyhow!("The owning directory is no longer known"))?;
            let expected = FsSnapshot::capture_file_contents(
                &owner_instance.metadata().relevant_paths,
                &self.vfs,
            )?;
            let snapshot = self.snapshot_owner(tree, owner)?;
            let disk_patch = compute_shallow_patch_set(snapshot.clone(), tree, owner);
            if context.stale || disk_patch != PatchSet::new() {
                if authority == WriteAuthority::Filesystem {
                    return Ok(InlineRemoval::Restore(Box::new(snapshot)));
                }
                // The owner's file is rewritten from the server's copy, so it
                // takes in the disk's changes first to keep them.
                catch_up(tree, disk_patch, context.batch, messages);
                if tree.get_instance(id).is_none() {
                    return Ok(InlineRemoval::Removed);
                }
            }
            let new_tree = inline_subtree(tree, owner, &InlineEdit::Remove(id));
            let staged = syncback_instance(&self.vfs, tree, &self.project(), owner, new_tree)?;
            validate_write_targets(&self.project(), &staged)?;
            let latest = self.snapshot_owner(tree, owner)?;
            if snapshots_differ(snapshot, latest.as_ref()) {
                if authority == WriteAuthority::Filesystem {
                    return Ok(InlineRemoval::Restore(Box::new(latest)));
                }
                continue;
            }
            match staged.write_single_file_to_vfs(
                self.project().folder_location(),
                &self.vfs,
                &expected,
            ) {
                Ok(()) => return Ok(InlineRemoval::Removed),
                // The file changed after the last read, which is the same race.
                Err(err)
                    if authority == WriteAuthority::Studio
                        && err.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue
                }
                Err(err) => return Err(err.into()),
            }
        }
        anyhow::bail!("The file kept changing on disk while the deletion was being saved")
    }

    fn persist_structural_update(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        outcome: &mut WriteOutcome,
        context: &WriteContext,
    ) {
        let inline_name = inline_owner(tree, update.id).is_some();
        let mut remaining = update.clone();
        remaining.changed_parent = None;
        let result = self.relocate_instance(tree, update, outcome.messages, context);
        match result {
            Ok(RelocationOutcome::Restore(snapshot)) => {
                if snapshot.is_none() && context.authority == WriteAuthority::Studio {
                    outcome.errors.push(removed_on_disk(update.id));
                }
                let restoration = filesystem_restoration(tree, update, *snapshot, false);
                outcome
                    .conflicts
                    .removed_instances
                    .extend(restoration.removed_instances);
                outcome
                    .conflicts
                    .added_instances
                    .extend(restoration.added_instances);
                outcome
                    .conflicts
                    .updated_instances
                    .extend(restoration.updated_instances);
                return;
            }
            Ok(RelocationOutcome::Moved) => {
                let instance = tree.get_instance(update.id).unwrap();
                let natural_name = update.changed_name.as_deref() == Some(instance.name());
                let saved = PatchUpdate {
                    changed_parent: update.changed_parent.map(|_| instance.parent()),
                    changed_name: natural_name.then(|| instance.name().to_owned()),
                    ..empty_update(update.id)
                };
                if saved.changed_parent.is_some() || saved.changed_name.is_some() {
                    outcome.accepted.updated_instances.push(saved);
                }
                if natural_name {
                    remaining.changed_name = None;
                }
            }
            Err(error) => {
                outcome.errors.push(WriteError {
                    id: update.id,
                    property: Some(
                        if update.changed_parent.is_some() {
                            "Parent"
                        } else {
                            "Name"
                        }
                        .into(),
                    ),
                    message: format!("{error:#}"),
                });
                if let Some(instance) = tree.get_instance(update.id) {
                    outcome.conflicts.updated_instances.push(PatchUpdate {
                        changed_parent: update.changed_parent.map(|_| instance.parent()),
                        changed_name: update
                            .changed_name
                            .as_ref()
                            .filter(|_| !inline_name)
                            .map(|_| instance.name().to_owned()),
                        ..empty_update(update.id)
                    });
                }
                if !inline_name {
                    remaining.changed_name = None;
                }
            }
        }
        // A filename rename may leave a metadata Name override in force. The scoped
        // serializer must clear that override before Name can be acknowledged.
        // Suppress structural dispatch on this second pass, which writes at the new path.
        if remaining.changed_name.is_some()
            || !remaining.changed_properties.is_empty()
            || remaining.changed_class_name.is_some()
            || remaining.changed_metadata.is_some()
        {
            self.persist_property_update(tree, &remaining, outcome, context, 1);
        }
    }

    fn relocate_instance(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        messages: &mut Vec<AppliedPatchSet>,
        context: &WriteContext,
    ) -> anyhow::Result<RelocationOutcome> {
        use crate::syncback::{
            promotion::plan_promotion,
            relocation::{cache_directory, execute_promotion, execute_relocation, plan_relocation},
            relocation_transaction::{Promotion, Relocation},
        };
        use anyhow::{ensure, Context as _};
        let id = update.id;
        ensure!(id != tree.get_root_id(), "Cannot relocate the tree root");
        ensure!(
            inline_owner(tree, id).is_none(),
            "Inline instances cannot move across owners"
        );
        let instance = tree.get_instance(id).unwrap();
        if let Some(reason) =
            package_reparent_block(tree, id, update.changed_parent.unwrap_or(instance.parent()))
        {
            anyhow::bail!(reason);
        }
        let source = match instance.metadata().instigating_source.as_ref() {
            Some(InstigatingSource::Path(path)) => path.clone(),
            _ => anyhow::bail!("Project-defined sources cannot be relocated"),
        };
        let middleware = instance
            .metadata()
            .middleware
            .context("Unknown source middleware")?;
        ensure!(
            !matches!(
                middleware,
                Middleware::Project | Middleware::Rbxm | Middleware::Rbxmx | Middleware::JsonModel
            ),
            "Project and model sources cannot be relocated"
        );
        let source_context = instance.metadata().context.clone();
        let snapshot = snapshot_from_vfs(&source_context, &self.vfs, &source)?;
        if snapshot.is_none() {
            return Ok(RelocationOutcome::Restore(Box::new(None)));
        }
        let disk_changes = compute_patch_set(snapshot.clone(), tree, id);
        if context.stale || disk_changes != PatchSet::new() {
            if context.authority == WriteAuthority::Filesystem {
                return Ok(RelocationOutcome::Restore(Box::new(snapshot)));
            }
            catch_up(tree, disk_changes, context.batch, messages);
        }
        let instance = tree
            .get_instance(id)
            .context("The source was removed on disk")?;
        let parent = update.changed_parent.unwrap_or(instance.parent());
        ensure!(parent != id, "An instance cannot parent itself");
        let destination_parent = tree
            .get_instance(parent)
            .context("Unknown destination parent")?;
        let mut ancestor = parent;
        while ancestor.is_some() {
            ensure!(ancestor != id, "Reparenting would create a cycle");
            ancestor = tree
                .get_instance(ancestor)
                .context("Unknown ancestor")?
                .parent();
        }
        let destination_context = destination_parent.metadata().context.clone();
        // The root of a project, like the DataModel a place project describes,
        // records the project file as its source. It is a directory through its
        // tree's `$path`, never a script to promote.
        let destination_is_project_root =
            destination_parent.metadata().middleware == Some(Middleware::Project);
        let promotion = if parent != instance.parent() {
            match destination_parent.metadata().instigating_source.as_ref() {
                Some(InstigatingSource::Path(path))
                    if !destination_is_project_root
                        && !destination_parent
                            .metadata()
                            .middleware
                            .is_some_and(|value| value.is_dir()) =>
                {
                    let middleware = destination_parent
                        .metadata()
                        .middleware
                        .context("Unknown destination middleware")?;
                    ensure!(
                        matches!(
                            destination_parent.class_name().as_str(),
                            "Script" | "LocalScript" | "ModuleScript"
                        ),
                        "Only managed script destinations can be promoted"
                    );
                    let paths = plan_promotion(path, middleware)?;
                    ensure!(
                        destination_context
                            .get_user_sync_rule(&paths.directory)
                            .is_none(),
                        "Custom destination directories cannot be promoted"
                    );
                    ensure!(
                        !crate::snapshot::is_path_ignored(
                            &destination_context.path_ignore_rules,
                            &paths.directory
                        ),
                        "The promotion directory is ignored"
                    );
                    ensure!(
                        tree.get_ids_at_path(&paths.directory).is_empty(),
                        "Aliased destination directories cannot be promoted"
                    );
                    let current = snapshot_from_vfs(&destination_context, &self.vfs, path)?
                        .context("The promotion destination is no longer visible")?;
                    ensure!(
                        compute_patch_set(Some(current), tree, parent) == PatchSet::new(),
                        "The promotion destination changed; refresh before relocating"
                    );
                    for (source, target) in &paths.originals {
                        ensure!(
                            tree.get_ids_at_path(source).iter().all(|&id| id == parent),
                            "Aliased destinations cannot be promoted"
                        );
                        ensure!(
                            destination_context.get_user_sync_rule(source).is_none()
                                && destination_context.get_user_sync_rule(target).is_none(),
                            "Custom destination mappings cannot be promoted"
                        );
                        ensure!(
                            !crate::snapshot::is_path_ignored(
                                &destination_context.path_ignore_rules,
                                target
                            ),
                            "The promotion destination is ignored"
                        );
                        let mut targets = FsSnapshot::new();
                        targets.remove_file(source);
                        targets.add_file(target, Vec::new());
                        validate_write_targets(&self.project(), &targets)?;
                    }
                    // A project alias can refer to a file without owning its primary
                    // metadata index, so also inspect explicit project mappings.
                    for candidate in tree.descendants(tree.get_root_id()) {
                        if let Some(InstigatingSource::ProjectNode { node, .. }) =
                            candidate.metadata().instigating_source.as_ref()
                        {
                            if let Some(mapping) = &node.path {
                                ensure!(
                                    !paths
                                        .originals
                                        .iter()
                                        .any(|(source, _)| mapping.path() == source)
                                        && mapping.path() != paths.directory,
                                    "Project-defined destinations cannot be promoted"
                                );
                            }
                        }
                    }
                    ensure!(
                        self.vfs
                            .snapshot_tree(&paths.directory)
                            .with_not_found()?
                            .is_none(),
                        "The promotion directory is occupied"
                    );
                    Some(paths)
                }
                _ => None,
            }
        } else {
            None
        };
        let destination_directory = if let Some(promotion) = &promotion {
            promotion.directory.clone()
        } else if parent == instance.parent() {
            source
                .parent()
                .context("The source has no parent directory")?
                .to_path_buf()
        } else {
            match destination_parent.metadata().instigating_source.as_ref() {
                Some(InstigatingSource::Path(path))
                    if destination_parent
                        .metadata()
                        .middleware
                        .is_some_and(|middleware| middleware.is_dir()) =>
                {
                    path.clone()
                }
                Some(InstigatingSource::Path(project_file)) if destination_is_project_root => {
                    let project = Project::load_exact(
                        &self.vfs,
                        project_file,
                        Some(destination_parent.name()),
                    )?;
                    let mapping = project
                        .tree
                        .path
                        .as_ref()
                        .context("The project-defined destination has no directory $path")?;
                    project.folder_location().join(mapping.path())
                }
                Some(InstigatingSource::ProjectNode { node, .. }) => node
                    .path
                    .as_ref()
                    .context("The project-defined destination has no directory $path")?
                    .path()
                    .to_path_buf(),
                _ => anyhow::bail!(
                    "The destination must be a managed directory or a promotable script"
                ),
            }
        };
        if promotion.is_none() {
            ensure!(
                self.vfs.metadata(&destination_directory)?.is_dir(),
                "The destination $path is not an existing directory"
            );
        }
        if parent != instance.parent() && promotion.is_none() {
            let destination_snapshot =
                snapshot_shallow_from_vfs(&destination_context, &self.vfs, &destination_directory)?
                    .context("The destination directory is no longer visible")?;
            ensure!(
                destination_snapshot.metadata.middleware != Some(Middleware::Project),
                "The destination directory now defines its own project"
            );
            // A project root reads as a plain directory through its `$path`, like
            // any other project node, so only file-backed destinations compare.
            if !destination_is_project_root
                && matches!(
                    destination_parent.metadata().instigating_source,
                    Some(InstigatingSource::Path(_))
                )
            {
                ensure!(
                    destination_snapshot.metadata.middleware
                        == destination_parent.metadata().middleware,
                    "The destination's filesystem middleware changed; refresh before relocating"
                );
            }
        }
        let plan = plan_relocation(
            &source,
            middleware,
            instance.name(),
            update.changed_name.as_deref().unwrap_or(instance.name()),
            &destination_directory,
        )?;
        ensure!(
            destination_parent
                .children()
                .iter()
                .all(|&child| child == id
                    || tree.get_instance(child).unwrap().name()
                        != update.changed_name.as_deref().unwrap_or(instance.name())),
            "The destination already contains an instance with this name"
        );
        if !middleware.is_dir() {
            let rules = crate::snapshot_middleware::default_sync_rules();
            let source_rule = rules
                .iter()
                .find(|rule| rule.matches(&source))
                .context("The source has no default filesystem mapping")?;
            let destination_rule = rules
                .iter()
                .find(|rule| rule.matches(&plan.destination))
                .context("The destination would no longer represent an instance")?;
            ensure!(
                source_rule.middleware == destination_rule.middleware,
                "Relocation would change the source's filesystem middleware"
            );
        }
        let old_instance_path = tree.inner().full_path_of(id, "/");
        let parent_path = tree.inner().full_path_of(parent, "/");
        let new_instance_path = if parent_path.is_empty() {
            update
                .changed_name
                .as_deref()
                .unwrap_or(instance.name())
                .to_owned()
        } else {
            format!(
                "{parent_path}/{}",
                update.changed_name.as_deref().unwrap_or(instance.name())
            )
        };
        let subtree: HashSet<_> = tree.descendants(id).map(|instance| instance.id()).collect();
        for instance in tree.descendants(tree.get_root_id()) {
            if let Some(InstigatingSource::ProjectNode { node, .. }) =
                instance.metadata().instigating_source.as_ref()
            {
                if let Some(path) = node.path.as_ref() {
                    ensure!(
                        !path.path().starts_with(&source),
                        "Relocation would break a project $path mapping"
                    );
                }
            }
            if subtree.contains(&instance.id()) {
                ensure!(
                    !matches!(
                        instance.metadata().instigating_source,
                        Some(InstigatingSource::ProjectNode { .. })
                    ),
                    "Directories containing project-defined sources cannot be relocated"
                );
                if let Some(rules) = &self.project().syncback_rules {
                    let old_path = tree.inner().full_path_of(instance.id(), "/");
                    let suffix = old_path.strip_prefix(&old_instance_path).unwrap();
                    let new_path = format!("{new_instance_path}{suffix}");
                    ensure!(
                        !rules
                            .ignore_trees
                            .iter()
                            .any(|ignored| old_path.starts_with(ignored)
                                || new_path.starts_with(ignored)),
                        "The relocation involves an excluded instance tree"
                    );
                }
                ensure!(
                    instance
                        .metadata()
                        .context
                        .get_user_sync_rule(
                            instance
                                .metadata()
                                .instigating_source
                                .as_ref()
                                .map(|source| source.path())
                                .unwrap_or(&source)
                        )
                        .is_none(),
                    "Custom source mappings cannot be safely relocated"
                );
                for path in &instance.metadata().relevant_paths {
                    ensure!(
                        tree.get_ids_at_path(path)
                            .iter()
                            .all(|id| subtree.contains(id)),
                        "Aliased filesystem sources cannot be relocated"
                    );
                    if let Ok(suffix) = path.strip_prefix(&source) {
                        let target = plan.destination.join(suffix);
                        ensure!(
                            !crate::snapshot::is_path_ignored(
                                &destination_context.path_ignore_rules,
                                &target
                            ),
                            "The destination excludes a moved source"
                        );
                        ensure!(
                            destination_context.get_user_sync_rule(&target).is_none(),
                            "The destination uses a custom source mapping"
                        );
                        if self.vfs.metadata(path).with_not_found()?.is_some() {
                            let mut targets = FsSnapshot::new();
                            targets.remove_file(path);
                            targets.add_file(&target, Vec::new());
                            validate_write_targets(&self.project(), &targets)?;
                        }
                    }
                }
            }
        }
        if source == plan.destination {
            ensure!(
                parent == instance.parent(),
                "Distinct instance parents alias the same filesystem directory"
            );
            return Ok(RelocationOutcome::Moved);
        }
        let mut moves = Vec::new();
        let mut absent_paths = Vec::new();
        if let Some(promotion) = &promotion {
            for (source, destination) in &promotion.originals {
                ensure!(
                    plan.destination != *destination
                        && plan
                            .sidecars
                            .iter()
                            .all(|(_, target)| target != destination),
                    "The child would overwrite a promotion initializer or sidecar"
                );
                if let Some(expected) = self.vfs.snapshot_tree(source).with_not_found()? {
                    moves.push(Relocation {
                        source: source.clone(),
                        destination: destination.clone(),
                        expected,
                    });
                } else {
                    absent_paths.extend([source.clone(), destination.clone()]);
                }
            }
        }
        for (source, destination) in
            std::iter::once((plan.source.clone(), plan.destination.clone()))
                .chain(plan.sidecars.clone())
        {
            // Even an absent sidecar's destination must be free, or it would attach
            // unrelated metadata to the relocated instance.
            ensure!(
                self.vfs.metadata(&destination).with_not_found()?.is_none(),
                "A relocation destination is occupied"
            );
            ensure!(
                !crate::snapshot::is_path_ignored(
                    &destination_context.path_ignore_rules,
                    &destination
                ),
                "The destination is ignored"
            );
            ensure!(
                destination_context
                    .get_user_sync_rule(&destination)
                    .is_none(),
                "The destination uses a custom source mapping"
            );
            let mut targets = FsSnapshot::new();
            targets.remove_file(&source);
            targets.add_file(&destination, Vec::new());
            validate_write_targets(&self.project(), &targets)?;
            if let Some(expected) = self.vfs.snapshot_tree(&source).with_not_found()? {
                moves.push(Relocation {
                    source,
                    destination,
                    expected,
                });
            } else {
                absent_paths.extend([source, destination]);
            }
        }
        // Re-read after planning. The transaction additionally checks exact bytes,
        // including unmanaged directory contents, immediately before staging them.
        let latest = snapshot_from_vfs(&source_context, &self.vfs, &source)?;
        ensure!(
            !snapshots_differ(snapshot, latest.as_ref()),
            "The source changed while planning its relocation"
        );
        let cache = cache_directory(&self.project());
        ensure!(
            !cache.starts_with(&source)
                && promotion.as_ref().is_none_or(|promotion| {
                    !cache.starts_with(&promotion.directory)
                        && !promotion.directory.starts_with(&cache)
                }),
            "The project's state directory cannot be relocated or promoted"
        );
        self.vfs.create_dir_all(&cache)?;
        self.vfs.sync_path(&cache)?;
        self.vfs.sync_path(cache.parent().unwrap())?;
        self.vfs.sync_path(self.project().folder_location())?;
        let journal = cache.join(uuid::Uuid::new_v4().to_string());
        let rollback = moves
            .iter()
            .map(|entry| Relocation {
                source: entry.destination.clone(),
                destination: entry.source.clone(),
                expected: entry.expected.clone(),
            })
            .collect();
        if promotion.is_some() {
            execute_promotion(
                &self.vfs,
                &journal,
                Promotion {
                    destination: destination_directory.clone(),
                    originals: moves,
                },
                absent_paths,
            )?;
        } else {
            execute_relocation(&self.vfs, &journal, moves, absent_paths)?;
        }
        let destination = snapshot_from_vfs(&destination_context, &self.vfs, &plan.destination)
            .and_then(|snapshot| {
                snapshot.context("The relocated source is not visible at the destination")
            });
        let destination = match destination {
            Ok(snapshot) => snapshot,
            Err(error) if promotion.is_some() => {
                // Do not reverse committed output as disjoint moves. It contains
                // nested paths; preserve it for filesystem snapshot reconciliation.
                return Err(error.context(format!(
                    "Committed promotion output requires attention at {}",
                    destination_directory.display()
                )));
            }
            Err(error) => {
                let rollback_journal = cache.join(uuid::Uuid::new_v4().to_string());
                execute_relocation(&self.vfs, &rollback_journal, rollback, Vec::new())
                    .context("The destination could not be read and rollback needs attention")?;
                return Err(error);
            }
        };
        let promoted = if promotion.is_some() {
            Some(
                snapshot_from_vfs(&destination_context, &self.vfs, &destination_directory)?
                    .context("The promoted parent is no longer visible")?,
            )
        } else {
            None
        };
        if let Some(promoted) = &promoted {
            tree.update_metadata(parent, promoted.metadata.clone());
        }
        // Update every physical path before applying the destination snapshot. Matching
        // children still uses their original names, preserving all existing referents.
        let metadata: Vec<_> = tree
            .descendants(id)
            .map(|instance| {
                let mut metadata = instance.metadata().clone();
                if let Some(InstigatingSource::Path(path)) = &mut metadata.instigating_source {
                    if let Ok(suffix) = path.strip_prefix(&source) {
                        *path = if suffix.as_os_str().is_empty() {
                            plan.destination.clone()
                        } else {
                            plan.destination.join(suffix)
                        };
                    }
                }
                for path in &mut metadata.relevant_paths {
                    if let Ok(suffix) = path.strip_prefix(&source) {
                        *path = if suffix.as_os_str().is_empty() {
                            plan.destination.clone()
                        } else {
                            plan.destination.join(suffix)
                        };
                    } else if let Some((_, destination)) =
                        plan.sidecars.iter().find(|(old, _)| old == path)
                    {
                        *path = destination.clone();
                    }
                }
                metadata.context = destination_context.clone();
                (instance.id(), metadata)
            })
            .collect();
        for (id, metadata) in metadata {
            tree.update_metadata(id, metadata);
        }
        let moved = PatchUpdate {
            changed_parent: update.changed_parent,
            ..empty_update(id)
        };
        let applied = apply_patch_set(
            tree,
            PatchSet {
                updated_instances: vec![moved],
                ..PatchSet::new()
            },
        );
        if !applied.is_empty() {
            messages.push(applied);
        }
        let applied = apply_patch_set(tree, compute_patch_set(Some(destination), tree, id));
        if !applied.is_empty() {
            messages.push(applied);
        }
        if let Some(promoted) = promoted {
            // Refresh the entire promoted owner after reparenting existing referents,
            // rather than reifying the output snapshot with fresh child identities.
            let applied = apply_patch_set(tree, compute_patch_set(Some(promoted), tree, parent));
            if !applied.is_empty() {
                messages.push(applied);
            }
        }
        Ok(RelocationOutcome::Moved)
    }

    fn persist_update(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        outcome: &mut WriteOutcome,
        context: &WriteContext,
        attempt: u32,
    ) {
        // A move that touches a file shared with other instances rewrites
        // those files instead of moving files of its own.
        let stored_move = tree.get_instance(update.id).is_some_and(|instance| {
            update.changed_parent.is_some_and(|parent| {
                parent != instance.parent()
                    && (stored_in(tree, update.id).is_some()
                        || children_stored_in(tree, parent).is_some())
            })
        });
        if stored_move {
            self.persist_stored_update(tree, update, outcome, context);
            return;
        }
        let structural = tree.get_instance(update.id).is_some_and(|instance| {
            update
                .changed_parent
                .is_some_and(|parent| parent != instance.parent())
                || (stored_in(tree, update.id).is_none()
                    && !matches!(
                        instance.metadata().middleware,
                        Some(Middleware::Rbxm | Middleware::Rbxmx)
                    )
                    && update
                        .changed_name
                        .as_deref()
                        .is_some_and(|name| name != instance.name()))
        });
        if structural {
            self.persist_structural_update(tree, update, outcome, context);
            return;
        }
        self.persist_property_update(tree, update, outcome, context, attempt);
    }

    /// Persists a move that touches a shared file, then any other edits in the
    /// same update.
    fn persist_stored_update(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        outcome: &mut WriteOutcome,
        context: &WriteContext,
    ) {
        let id = update.id;
        let restore = |tree: &RojoTree, outcome: &mut WriteOutcome| {
            if let Some(instance) = tree.get_instance(id) {
                outcome.conflicts.updated_instances.push(PatchUpdate {
                    changed_parent: Some(instance.parent()),
                    changed_name: update
                        .changed_name
                        .as_ref()
                        .map(|_| instance.name().to_owned()),
                    ..empty_update(id)
                });
            }
        };
        // Whatever happens to the move, the property edits that came with it
        // go through the normal path, which saves or restores each of them.
        let remaining = PatchUpdate {
            changed_parent: None,
            changed_name: None,
            ..update.clone()
        };
        let persist_remaining = |tree: &mut RojoTree, outcome: &mut WriteOutcome| {
            if !remaining.changed_properties.is_empty() && tree.get_instance(id).is_some() {
                self.persist_property_update(tree, &remaining, outcome, context, 1);
            }
        };
        let destination = update
            .changed_parent
            .and_then(|parent| children_stored_in(tree, parent));
        let mut files: Vec<StoredIn> = stored_in(tree, id).into_iter().chain(destination).collect();
        files.dedup();
        for stored in files {
            match self.catch_up_stored(tree, stored, context, outcome.messages, None) {
                Ok(true) => {}
                Ok(false) => {
                    restore(tree, outcome);
                    persist_remaining(tree, outcome);
                    return;
                }
                Err(error) => {
                    outcome.errors.push(WriteError {
                        id,
                        property: Some("Parent".into()),
                        message: format!("Could not read a shared file: {error:#}"),
                    });
                    restore(tree, outcome);
                    persist_remaining(tree, outcome);
                    return;
                }
            }
        }
        if tree.get_instance(id).is_none() {
            if context.authority == WriteAuthority::Studio {
                outcome.errors.push(removed_on_disk(id));
            }
            return;
        }
        match self.persist_stored_move(tree, update, context) {
            Ok(unsaved) => {
                let instance = tree.get_instance(id);
                let moved = PatchUpdate {
                    changed_parent: instance.map(|instance| instance.parent()),
                    changed_name: update.changed_name.clone(),
                    ..empty_update(id)
                };
                outcome.messages.push(saved_change_message(tree, &moved));
                outcome.accepted.updated_instances.push(moved);
                outcome.errors.extend(unsaved);
                persist_remaining(tree, outcome);
            }
            Err(error) => {
                outcome.errors.push(WriteError {
                    id,
                    property: Some("Parent".into()),
                    message: format!("{error:#}"),
                });
                restore(tree, outcome);
                persist_remaining(tree, outcome);
            }
        }
    }

    /// Persists an edit to an instance in a shared file by rewriting the file:
    /// a model file, or a directory's metadata when the edit sets references,
    /// which only a rewrite can link.
    fn persist_stored_edit(
        &self,
        tree: &mut RojoTree,
        stored: StoredIn,
        update: &PatchUpdate,
        outcome: &mut WriteOutcome,
        context: &WriteContext,
    ) {
        let id = update.id;
        let restore = |tree: &RojoTree, outcome: &mut WriteOutcome| {
            if let Some(instance) = tree.get_instance(id) {
                outcome.conflicts.updated_instances.push(PatchUpdate {
                    changed_name: update
                        .changed_name
                        .as_ref()
                        .map(|_| instance.name().to_owned()),
                    changed_properties: update
                        .changed_properties
                        .keys()
                        .map(|key| (*key, instance.properties().get(key).cloned()))
                        .collect(),
                    ..empty_update(id)
                });
            }
        };

        // Ordinary properties merge onto current shared-file contents. Structural and
        // payload edits retain the session's filesystem-authoritative conflict policy.
        match self.catch_up_stored(tree, stored, context, outcome.messages, Some(update)) {
            Ok(true) => {}
            Ok(false) => {
                restore(tree, outcome);
                return;
            }
            Err(error) => {
                outcome.errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Could not read the model file: {error:#}"),
                });
                return;
            }
        }
        let Some(instance) = tree.get_instance(id) else {
            if context.authority == WriteAuthority::Studio {
                outcome.errors.push(removed_on_disk(id));
            }
            return;
        };

        // Properties that other two-way edits refuse are refused here too, and
        // Studio gets their saved values back. The rest are still saved.
        let class = instance.class_name();
        let filter_tree = WeakDom::new(InstanceBuilder::new(class));
        let ignored = get_property_filter(&self.project(), filter_tree.root()).unwrap_or_default();
        let mut refused = UstrMap::new();
        let mut allowed = update.clone();
        allowed.changed_properties.retain(|key, _| {
            let message = if !can_write_property(&self.project(), class.as_str(), key.as_str()) {
                "This property is excluded by reflection serialization rules"
            } else if ignored.contains(key) {
                "This property is excluded by the project's syncback rules"
            } else {
                return true;
            };
            outcome.errors.push(WriteError {
                id,
                property: Some(key.to_string()),
                message: message.into(),
            });
            refused.insert(*key, instance.properties().get(key).cloned());
            false
        });
        if !refused.is_empty() {
            outcome.conflicts.updated_instances.push(PatchUpdate {
                changed_properties: refused,
                ..empty_update(id)
            });
        }
        if allowed.changed_name.is_none() && allowed.changed_properties.is_empty() {
            return;
        }
        let update = &allowed;

        let result = (|| -> anyhow::Result<Vec<WriteError>> {
            anyhow::ensure!(
                update.changed_class_name.is_none() && update.changed_metadata.is_none(),
                "Changing the class or metadata is not supported by two-way sync"
            );
            let mut copy = StoredCopy::new(tree, stored);
            let target = copy
                .copy_of(id)
                .context("The instance isn't stored in this model")?;
            // A reference to an instance in the model points at its copy.
            let values: Vec<_> = update
                .changed_properties
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        Some(Variant::Ref(referent)) => {
                            Some(Variant::Ref(copy.copy_of(*referent).unwrap_or(*referent)))
                        }
                        other => other.clone(),
                    };
                    (*key, value)
                })
                .collect();
            let instance = copy.dom.get_by_ref_mut(target).unwrap();
            if let Some(name) = &update.changed_name {
                instance.name = name.clone();
            }
            for (key, value) in values {
                // Studio never sees the attributes that link references, so
                // the ones the instance has are kept.
                let value = if key == "Attributes" {
                    let edited = value.unwrap_or_else(|| Attributes::new().into());
                    Some(merge_link_attributes(
                        &edited,
                        instance.properties.get(&key),
                    ))
                } else {
                    value
                };
                match value {
                    Some(value) => instance.properties.insert(key, value),
                    None => instance.properties.remove(&key),
                };
            }
            let written = self.write_stored(tree, stored, copy)?;
            Ok(Self::apply_stored(tree, written, &[]))
        })();
        match result {
            Ok(unsaved) => {
                outcome.accepted.updated_instances.push(update.clone());
                outcome.messages.push(saved_change_message(tree, update));
                outcome.errors.extend(unsaved);
            }
            Err(error) => {
                outcome.errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("{error:#}"),
                });
                restore(tree, outcome);
            }
        }
    }

    /// Brings the tree in line with a shared file that changed on disk, and
    /// tells Studio. Returns whether the write should go ahead: always when
    /// the file hadn't changed, and only under Studio authority when it had.
    fn catch_up_stored(
        &self,
        tree: &mut RojoTree,
        stored: StoredIn,
        context: &WriteContext,
        messages: &mut Vec<AppliedPatchSet>,
        property_update: Option<&PatchUpdate>,
    ) -> anyhow::Result<bool> {
        if !context.stale && self.stored_matches_disk(tree, stored)? {
            return Ok(true);
        }
        let owner = stored.owner();
        let instance = tree.get_instance(owner).unwrap();
        let Some(InstigatingSource::Path(source)) = instance.metadata().instigating_source.as_ref()
        else {
            anyhow::bail!("The file's owner has no filesystem source");
        };
        let context_for_read = instance.metadata().context.clone();
        let source = source.clone();
        let disk_patch = match stored {
            StoredIn::Inline(_) => compute_shallow_patch_set(
                snapshot_shallow_from_vfs(&context_for_read, &self.vfs, &source)?,
                tree,
                owner,
            ),
            StoredIn::Model(_) => compute_patch_set(
                snapshot_from_vfs(&context_for_read, &self.vfs, &source)?,
                tree,
                owner,
            ),
        };
        let authority = property_update.map_or(context.authority, |update| {
            self.property_write_authority(tree, update, context, &disk_patch)
        });
        // A global cursor can be old because of an earlier drag update, not a disk
        // conflict. Structural and payload writes still require the original baseline.
        if authority == WriteAuthority::Filesystem
            && (context.stale || disk_patch != PatchSet::new())
        {
            let applied = apply_patch_set(tree, disk_patch);
            if !applied.is_empty() {
                messages.push(applied);
            }
            return Ok(false);
        }
        if disk_patch == PatchSet::new() {
            return Ok(true);
        }
        catch_up(tree, disk_patch, context.batch, messages);
        Ok(true)
    }

    fn property_write_authority(
        &self,
        tree: &RojoTree,
        update: &PatchUpdate,
        context: &WriteContext,
        disk_changes: &PatchSet,
    ) -> WriteAuthority {
        let Some(instance) = tree.get_instance(update.id) else {
            return context.authority;
        };
        if context.authority != WriteAuthority::Filesystem
            || !is_ordinary_property_update(
                update,
                instance.class_name().as_str(),
                instance.metadata().middleware,
            )
            || changes_require_context(tree, disk_changes)
            || disk_changes.updated_instances.iter().any(|change| {
                change.changed_metadata.as_ref().is_some_and(|metadata| {
                    !tree
                        .get_metadata(change.id)
                        .unwrap()
                        .has_same_structure(metadata)
                })
            })
        {
            return context.authority;
        }
        let Some(cursor) = context.cursor else {
            return context.authority;
        };
        let targets = std::iter::successors(tree.get_instance(update.id), |instance| {
            tree.get_instance(instance.parent())
        })
        .map(|instance| instance.id());
        let structural = self
            .message_queue
            .has_structural_changes_since(cursor, targets);
        if structural == Some(false) {
            WriteAuthority::Studio
        } else {
            context.authority
        }
    }

    fn persist_property_update(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        outcome: &mut WriteOutcome,
        context: &WriteContext,
        attempt: u32,
    ) {
        // Model files store their whole subtree, so their edits rewrite it. A
        // reference set on an instance in a directory's metadata needs the
        // same rewrite, which links it through attributes.
        let sets_reference = update
            .changed_properties
            .values()
            .any(|value| matches!(value, Some(Variant::Ref(_))));
        let middleware = tree
            .get_metadata(update.id)
            .and_then(|metadata| metadata.middleware);
        let stored = match stored_in(tree, update.id) {
            Some(StoredIn::Model(model)) => Some(StoredIn::Model(model)),
            Some(StoredIn::Inline(owner)) if sets_reference => Some(StoredIn::Inline(owner)),
            _ if is_model_middleware(middleware) => Some(StoredIn::Model(update.id)),
            _ if sets_reference && middleware == Some(Middleware::Dir) => {
                Some(StoredIn::Inline(update.id))
            }
            _ => None,
        };
        if let Some(stored) = stored {
            self.persist_stored_edit(tree, stored, update, outcome, context);
            return;
        }
        let WriteOutcome {
            accepted,
            conflicts,
            errors,
            messages,
        } = outcome;
        let authority = context.authority;
        // A retry starts over, so it drops what the attempt before it reported.
        let errors_before = errors.len();
        let id = update.id;
        if tree.get_instance(id).is_none() {
            errors.push(WriteError {
                id,
                property: None,
                message: "Cannot update an unknown instance".into(),
            });
            return;
        }
        // Instances stored in a directory's init.meta.json are written, compared,
        // and restored through that directory.
        let inline_owner = inline_owner(tree, id);
        let owner_id = inline_owner.unwrap_or(id);
        let owner = tree.get_instance(owner_id).unwrap();
        let path = match owner.metadata().instigating_source.as_ref() {
            // Instances made by a project file can't be written back, and checking
            // them would re-snapshot the whole project, so they're rejected first.
            Some(InstigatingSource::Path(path))
                if owner.metadata().middleware != Some(Middleware::Project) =>
            {
                path
            }
            _ => {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: "Project-defined nodes cannot be edited through two-way sync".into(),
                });
                return;
            }
        };
        for (property, label) in [
            (update.changed_class_name.is_some(), "class name"),
            (update.changed_metadata.is_some(), "metadata"),
        ] {
            if property {
                errors.push(WriteError {
                    id,
                    property: Some(label.into()),
                    message: format!("Changing {label} is not supported by two-way sync"),
                });
            }
        }
        let expected =
            match FsSnapshot::capture_file_contents(&owner.metadata().relevant_paths, &self.vfs) {
                Ok(expected) => expected,
                Err(error) => {
                    errors.push(WriteError {
                        id,
                        property: None,
                        message: format!("Could not validate filesystem sources: {error:#}"),
                    });
                    return;
                }
            };
        // Only the owner's own file matters here, so its separately stored
        // children aren't read, which keeps writes to large folders fast.
        let snapshot = match snapshot_shallow_from_vfs(&owner.metadata().context, &self.vfs, path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Could not read current filesystem state: {err:#}"),
                });
                return;
            }
        };
        if snapshot.is_none() {
            conflicts.removed_instances.push(owner_id);
            if authority == WriteAuthority::Studio {
                errors.push(removed_on_disk(id));
            }
            return;
        }
        let path = path.clone();
        let disk_changes = compute_shallow_patch_set(snapshot.clone(), tree, owner_id);
        // A drag keeps its original cursor across several writes. Only unseen structural
        // changes invalidate that target; ordinary fields merge onto current disk values.
        let authority = self.property_write_authority(tree, update, context, &disk_changes);
        if context.stale || disk_changes != PatchSet::new() {
            if authority == WriteAuthority::Filesystem {
                let disk_patch = disk_restoration(tree, owner_id, update, snapshot);
                conflicts
                    .removed_instances
                    .extend(disk_patch.removed_instances);
                conflicts.added_instances.extend(disk_patch.added_instances);
                conflicts
                    .updated_instances
                    .extend(disk_patch.updated_instances);
                return;
            }
            // The server takes in the disk's changes, so the write below starts
            // from the current file and only Studio's edits replace anything.
            catch_up(tree, disk_changes, context.batch, messages);
            if tree.get_instance(id).is_none() {
                errors.push(removed_on_disk(id));
                return;
            }
        }
        let instance = tree.get_instance(id).unwrap();
        let owner = tree.get_instance(owner_id).unwrap();
        if update.changed_class_name.is_some() || update.changed_metadata.is_some() {
            let restoration = disk_restoration(tree, owner_id, update, snapshot);
            conflicts
                .updated_instances
                .extend(restoration.updated_instances);
            return;
        }
        if matches!(
            owner.metadata().middleware,
            Some(Middleware::Rbxm | Middleware::Rbxmx | Middleware::JsonModel)
        ) {
            errors.push(WriteError {
                id,
                property: None,
                message:
                    "Model-file updates require subtree serialization and are not supported yet"
                        .into(),
            });
            let restoration = disk_restoration(tree, owner_id, update, snapshot);
            conflicts
                .updated_instances
                .extend(restoration.updated_instances);
            return;
        }
        let database = rbx_reflection_database::get().unwrap();
        let defaults = database
            .classes
            .get(instance.class_name().as_str())
            .map(|class| &class.default_properties);
        let payload_property =
            file_payload_property(owner.class_name().as_str(), owner.metadata().middleware);
        let requested_update = update;
        let mut normalized_update = update.clone();
        for (key, value) in &mut normalized_update.changed_properties {
            if let Some(original) = value {
                let normalized = normalize_json_variant(original.clone());
                let default = defaults.and_then(|values| values.get(key.as_str()));
                *value = if same_value(Some(&normalized), default)
                    && key != "Source"
                    && Some(*key) != payload_property
                {
                    None
                } else {
                    Some(normalized)
                };
            }
        }
        let update = &normalized_update;
        // With no discrepancy on disk, an inline instance's values on the server
        // are exactly what its owner's file stores.
        let disk_root = snapshot.as_ref().unwrap();
        let (disk_name, disk_properties) = match inline_owner {
            Some(_) => (instance.name(), instance.properties()),
            None => (disk_root.name.as_ref() as &str, &disk_root.properties),
        };
        let mut normalization_corrections = UstrMap::new();
        let mut accepted_properties = UstrMap::new();
        let mut conflict_properties = UstrMap::new();
        let mut accepted_name = None;
        let mut conflict_name = None;
        if let Some(studio_name) = &update.changed_name {
            let baseline_name = Variant::String(instance.name().into());
            let filesystem_name = Variant::String(disk_name.into());
            let studio_name_variant = Variant::String(studio_name.clone());
            match decide_property_write(
                Some(&baseline_name),
                Some(&filesystem_name),
                Some(&studio_name_variant),
            ) {
                PropertyWriteDecision::AlreadySynchronized => {}
                PropertyWriteDecision::RestoreFilesystem => {
                    conflict_name = Some(disk_name.to_owned());
                }
                PropertyWriteDecision::Persist => {
                    accepted_name = Some(studio_name.clone());
                }
            }
        }
        // Syncback omits reflected defaults, including values rounded to zero.
        // An omitted default and its explicit value must compare identically.
        for (key, studio_value) in &update.changed_properties {
            let default = defaults.and_then(|values| values.get(key.as_str()));
            let baseline_value = instance.properties().get(key).or(default);
            let disk_value = disk_properties.get(key).or(default);
            match decide_property_write(
                baseline_value,
                disk_value,
                studio_value.as_ref().or(default),
            ) {
                PropertyWriteDecision::AlreadySynchronized => {
                    let requested = requested_update.changed_properties[key].as_ref();
                    // Compare exact finite values, but let serde's null representation
                    // keep unchanged NaNs from triggering redundant corrections.
                    if requested.is_some()
                        && requested != disk_value
                        && serde_json::to_value(requested).expect("Variants must serialize")
                            != serde_json::to_value(disk_value).expect("Variants must serialize")
                    {
                        normalization_corrections.insert(*key, disk_properties.get(key).cloned());
                    }
                }
                PropertyWriteDecision::RestoreFilesystem => {
                    conflict_properties.insert(*key, disk_value.cloned());
                }
                PropertyWriteDecision::Persist => {
                    if !can_write_property(
                        &self.project(),
                        instance.class_name().as_str(),
                        key.as_str(),
                    ) {
                        errors.push(WriteError {
                            id,
                            property: Some(key.to_string()),
                            message: "This property is excluded by reflection serialization rules"
                                .into(),
                        });
                        conflict_properties.insert(*key, disk_value.cloned());
                    } else if matches!(studio_value, Some(Variant::Ref(_) | Variant::UniqueId(_))) {
                        errors.push(WriteError {
                            id,
                            property: Some(key.to_string()),
                            message: "This reference cannot be serialized in this file format"
                                .into(),
                        });
                    } else if key == "Source" && !matches!(studio_value, Some(Variant::String(_))) {
                        errors.push(WriteError {
                            id,
                            property: Some(key.to_string()),
                            message: "Script source must be a string".into(),
                        });
                    } else {
                        accepted_properties.insert(*key, studio_value.clone());
                    }
                }
            }
        }
        let filter_tree = WeakDom::new(InstanceBuilder::new(instance.class_name()));
        if let Some(ignored) = get_property_filter(&self.project(), filter_tree.root()) {
            accepted_properties.retain(|key, _| {
                if ignored.contains(key) {
                    errors.push(WriteError {
                        id,
                        property: Some(key.to_string()),
                        message: "This property is excluded by the project's syncback rules".into(),
                    });
                    false
                } else {
                    true
                }
            });
        }
        for (key, studio_value) in &update.changed_properties {
            let default = defaults.and_then(|values| values.get(key.as_str()));
            let disk_value = disk_properties.get(key);
            if !accepted_properties.contains_key(key)
                && !same_value(disk_value.or(default), studio_value.as_ref().or(default))
            {
                conflict_properties.insert(*key, disk_value.cloned());
            }
        }
        if !conflict_properties.is_empty() || conflict_name.is_some() {
            conflicts.updated_instances.push(PatchUpdate {
                id,
                changed_name: conflict_name,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: conflict_properties,
                changed_metadata: None,
            });
        }
        if !normalization_corrections.is_empty() {
            messages.push(AppliedPatchSet {
                updated: vec![AppliedPatchUpdate {
                    changed_properties: normalization_corrections,
                    ..AppliedPatchUpdate::new(id)
                }],
                ..AppliedPatchSet::new()
            });
        }
        if accepted_properties.is_empty() && accepted_name.is_none() {
            return;
        }
        let mut properties = disk_properties.clone();
        for (key, value) in &accepted_properties {
            match value {
                Some(value) => properties.insert(*key, value.clone()),
                None => properties.remove(key),
            };
        }
        let name = accepted_name.as_deref().unwrap_or(disk_name);
        // A directory's metadata file also holds its inline descendants, so it is
        // rewritten from all of them rather than from the edited instance alone.
        let new_tree = if inline_owner.is_some() || has_inline_children(tree, id) {
            inline_subtree(
                tree,
                owner_id,
                &InlineEdit::Update {
                    id,
                    name,
                    properties: &properties,
                },
            )
        } else {
            WeakDom::new(
                InstanceBuilder::new(instance.class_name())
                    .with_name(name)
                    .with_properties(properties),
            )
        };
        let mut staged =
            match syncback_instance(&self.vfs, tree, &self.project(), owner_id, new_tree) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    errors.push(WriteError {
                        id,
                        property: None,
                        message: format!("Failed to serialize changes: {err:#}"),
                    });
                    conflicts.updated_instances.retain(|patch| patch.id != id);
                    conflicts.updated_instances.extend(
                        disk_restoration(tree, owner_id, update, snapshot).updated_instances,
                    );
                    return;
                }
            };
        if let Some(payload_property) = payload_property {
            let source = source_file(&owner);
            if !accepted_properties.contains_key(&payload_property) {
                // The payload may be script Source, text Value, or table Contents.
                // Directory-backed payloads live in their initializer, not the folder.
                staged.retain_added_files(|candidate| {
                    candidate != path && Some(candidate) != source.as_deref()
                });
            } else if let Some(source) = source
                .as_deref()
                .filter(|_| inline_owner.is_none() && accepted_name.is_none())
                .filter(|_| accepted_properties.len() == 1)
                .filter(|_| {
                    !expected.iter().any(|(path, bytes)| {
                        let metadata_file = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.ends_with(".meta.json") || name.ends_with(".meta.jsonc")
                            });
                        metadata_file
                            && bytes.as_ref().is_some_and(|bytes| {
                                crate::json::from_slice::<serde_json::Value>(bytes).map_or(
                                    true,
                                    |metadata| {
                                        metadata
                                            .get("properties")
                                            .and_then(|properties| {
                                                properties.get(payload_property.as_str())
                                            })
                                            .is_some()
                                    },
                                )
                            })
                    })
                })
            {
                // Preserve formatting unless metadata overrides the edited payload.
                // Such overrides must be removed by the serializer or the edit is lost.
                staged.retain_added_files(|candidate| candidate == source);
            }
        }
        if let Err(err) = validate_write_targets(&self.project(), &staged) {
            errors.push(WriteError {
                id,
                property: None,
                message: format!("{err:#}"),
            });
            conflicts.updated_instances.retain(|patch| patch.id != id);
            conflicts
                .updated_instances
                .extend(disk_restoration(tree, owner_id, update, snapshot).updated_instances);
            return;
        }
        // Every accepted value differs from the disk, so a write that changes no
        // file means the edit can't be represented and must not report Saved.
        if !staged.changes_files(self.project().folder_location(), &expected) {
            errors.push(WriteError {
                id,
                property: None,
                message: "These changes cannot be represented on disk".into(),
            });
            conflicts.updated_instances.retain(|patch| patch.id != id);
            conflicts
                .updated_instances
                .extend(disk_restoration(tree, owner_id, update, snapshot).updated_instances);
            return;
        }
        let latest = match snapshot_shallow_from_vfs(&owner.metadata().context, &self.vfs, &path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Could not revalidate filesystem state: {err:#}"),
                });
                return;
            }
        };
        let owner_context = owner.metadata().context.clone();
        // The file changing after it was read shows up either in the second
        // read or in the write's own byte check. Studio authority treats both as
        // the same race, while the filesystem keeps a failed write a failure.
        let written = (!snapshots_differ(snapshot, latest.as_ref())).then(|| {
            staged.write_single_file_to_vfs(self.project().folder_location(), &self.vfs, &expected)
        });
        match written {
            Some(Ok(())) => {
                let saved = PatchUpdate {
                    id,
                    changed_name: accepted_name,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_properties: accepted_properties,
                    changed_metadata: None,
                };
                // Apply requested renames first so reparsing the owner matches its
                // existing inline referents instead of replacing renamed children.
                let applied = apply_patch_set(
                    tree,
                    PatchSet {
                        updated_instances: vec![saved.clone()],
                        ..PatchSet::new()
                    },
                );
                if !applied.is_empty() {
                    messages.push(applied);
                }
                // Rewriting metadata can normalize untouched properties and inline
                // descendants too. Reconcile the entire owner before the next edit
                // so those changes cannot be mistaken for filesystem conflicts.
                let saved_snapshot =
                    match snapshot_shallow_from_vfs(&owner_context, &self.vfs, &path) {
                        Ok(snapshot) => snapshot,
                        Err(err) => {
                            errors.push(WriteError {
                                id,
                                property: None,
                                message: format!("Could not read saved filesystem state: {err:#}"),
                            });
                            return;
                        }
                    };
                let saved_changes = compute_saved_patch_set(saved_snapshot, tree, owner_id);
                let applied = apply_patch_set(tree, saved_changes);
                if !applied.is_empty() {
                    messages.push(applied);
                }
                accepted.updated_instances.push(saved);
                return;
            }
            Some(Err(err))
                if authority == WriteAuthority::Filesystem
                    || err.kind() != io::ErrorKind::WouldBlock =>
            {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Failed to persist changes: {err:#}"),
                });
            }
            _ => match changed_during_write(authority, attempt) {
                ChangedDuringWrite::Restore => {}
                ChangedDuringWrite::Retry => {
                    conflicts.updated_instances.retain(|patch| patch.id != id);
                    errors.truncate(errors_before);
                    let outcome = &mut WriteOutcome {
                        accepted,
                        conflicts,
                        errors,
                        messages,
                    };
                    return self.persist_property_update(
                        tree,
                        update,
                        outcome,
                        context,
                        attempt + 1,
                    );
                }
                ChangedDuringWrite::GiveUp => errors.push(WriteError {
                    id,
                    property: None,
                    message: "The file kept changing on disk while this edit was being saved"
                        .into(),
                }),
            },
        }
        // Read again, since the disk may have moved on since the last read.
        match snapshot_shallow_from_vfs(&owner_context, &self.vfs, &path) {
            Ok(latest) => {
                let restoration = disk_restoration(tree, owner_id, update, latest);
                conflicts.updated_instances.retain(|patch| patch.id != id);
                conflicts
                    .removed_instances
                    .extend(restoration.removed_instances);
                conflicts
                    .added_instances
                    .extend(restoration.added_instances);
                conflicts
                    .updated_instances
                    .extend(restoration.updated_instances);
            }
            Err(error) => errors.push(WriteError {
                id,
                property: None,
                message: format!("Could not restore filesystem state: {error:#}"),
            }),
        }
    }
}

/// Whether an instance's files can be deleted.
enum Removal {
    /// The files at this path can go.
    Allowed(PathBuf),
    /// The disk wins, and Studio gets back what it holds.
    Restore(Box<Option<crate::snapshot::InstanceSnapshot>>),
}

enum RelocationOutcome {
    Moved,
    Restore(Box<Option<InstanceSnapshot>>),
}

/// The contents of a shared file and its sidecars, captured before a write so
/// the write fails if they change.
type ExpectedFiles = HashMap<PathBuf, Option<Vec<u8>>>;

/// A shared file that was rewritten, as it was read back, with the ids its
/// instances are meant to keep.
struct WrittenStored {
    stored: StoredIn,
    snapshot: InstanceSnapshot,
    ids: AddedIds,
    outside: Vec<(Ref, Ustr)>,
}

/// Copies an instance and its descendants out of the tree, named `name`, as a
/// model with one top-level instance, along with the ids they keep.
fn subtree_model(tree: &RojoTree, id: Ref, name: &str) -> (WeakDom, AddedIds) {
    fn copy(
        tree: &RojoTree,
        id: Ref,
        dom: &mut WeakDom,
        parent: Ref,
        name: Option<&str>,
    ) -> AddedIds {
        let instance = tree.get_instance(id).unwrap();
        let name = name.unwrap_or(instance.name());
        let copied = dom.insert(
            parent,
            InstanceBuilder::new(instance.class_name())
                .with_name(name)
                .with_properties(properties_with_id(&instance)),
        );
        AddedIds {
            id,
            name: name.to_owned(),
            class_name: instance.class_name(),
            children: instance
                .children()
                .iter()
                .map(|child| copy(tree, *child, dom, copied, None))
                .collect(),
        }
    }
    let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
    let root = dom.root_ref();
    let ids = copy(tree, id, &mut dom, root, Some(name));
    let copies = model_copies(&dom, &ids);
    retarget_refs(&mut dom, &copies);
    (dom, ids)
}

/// Returns an instance's properties for a copy that's written back to disk.
/// An ID that only its metadata holds, like a `.model.json` root's `id`, goes
/// into a `Rojo_Id` attribute, so references to it survive the rewrite.
fn properties_with_id(instance: &InstanceWithMeta) -> UstrMap<Variant> {
    let mut properties = instance.properties().clone();
    if let Some(id) = &instance.metadata().specified_id {
        let attributes = properties
            .entry(ustr("Attributes"))
            .or_insert_with(|| Attributes::new().into());
        if let Variant::Attributes(attributes) = attributes {
            if attributes.get(REF_ID_ATTRIBUTE_NAME).is_none() {
                attributes.insert(
                    REF_ID_ATTRIBUTE_NAME.into(),
                    Variant::String(id.to_string()),
                );
            }
        }
    }
    properties
}

/// Maps the instances of a model with one top-level instance to the ids in
/// `ids`, which is shaped like it.
fn model_copies(model: &WeakDom, ids: &AddedIds) -> HashMap<Ref, Ref> {
    let mut copies = HashMap::new();
    let mut pending: Vec<_> = model.root().children().iter().copied().zip([ids]).collect();
    while let Some((copied, entry)) = pending.pop() {
        copies.insert(copied, entry.id);
        let children = model.get_by_ref(copied).unwrap().children().to_vec();
        pending.extend(children.into_iter().zip(entry.children.iter()));
    }
    copies
}

/// Stores references that leave `dom` as `Rojo_Target_*` attributes naming the
/// target's `Rojo_Id`, the way a full syncback links them across files. A
/// target without an ID is left as is, so [`outside_refs`] reports it.
/// `copies` maps each copy in `dom` to the tree instance it stands for.
fn link_outside_refs(tree: &RojoTree, dom: &mut WeakDom, copies: &HashMap<Ref, Ref>) {
    for &copy in copies.keys() {
        let Some(instance) = dom.get_by_ref(copy) else {
            continue;
        };
        let linked: Vec<(Ustr, String)> = instance
            .properties
            .iter()
            .filter_map(|(name, value)| match value {
                Variant::Ref(target) if target.is_some() && dom.get_by_ref(*target).is_none() => {
                    let id = tree.get_metadata(*target)?.specified_id.as_ref()?;
                    Some((*name, id.to_string()))
                }
                _ => None,
            })
            .collect();
        if linked.is_empty() {
            continue;
        }
        let instance = dom.get_by_ref_mut(copy).unwrap();
        let mut attributes = match instance.properties.remove(&ustr("Attributes")) {
            Some(Variant::Attributes(attributes)) => attributes,
            _ => Attributes::new(),
        };
        for (name, id) in linked {
            instance.properties.remove(&name);
            attributes.insert(
                format!("{REF_POINTER_ATTRIBUTE_PREFIX}{name}"),
                Variant::String(id),
            );
        }
        instance
            .properties
            .insert(ustr("Attributes"), Variant::Attributes(attributes));
    }
}

/// Returns the references a file can't hold, since they point at instances
/// outside `dom`, as the tree instance and property that hold each one.
/// `copies` maps each copy in `dom` to the tree instance it stands for.
fn outside_refs(dom: &WeakDom, copies: &HashMap<Ref, Ref>) -> Vec<(Ref, Ustr)> {
    let mut outside = Vec::new();
    for (&copy, &original) in copies {
        let Some(instance) = dom.get_by_ref(copy) else {
            continue;
        };
        for (name, value) in &instance.properties {
            if let Variant::Ref(target) = value {
                if target.is_some() && dom.get_by_ref(*target).is_none() {
                    outside.push((original, *name));
                }
            }
        }
    }
    outside
}

/// Tells every connected Studio about a change saved to a shared file, with
/// the values the tree holds now that the file was read back. Rewriting a
/// shared file keeps the tree in line with it, so the watcher has nothing left
/// to report.
fn saved_change_message(tree: &RojoTree, update: &PatchUpdate) -> AppliedPatchSet {
    let Some(instance) = tree.get_instance(update.id) else {
        return AppliedPatchSet::new();
    };
    AppliedPatchSet {
        updated: vec![AppliedPatchUpdate {
            id: update.id,
            changed_name: update
                .changed_name
                .as_ref()
                .map(|_| instance.name().to_owned()),
            changed_class_name: None,
            changed_parent: update.changed_parent.map(|_| instance.parent()),
            changed_properties: update
                .changed_properties
                .keys()
                .map(|key| (*key, instance.properties().get(key).cloned()))
                .collect(),
            changed_metadata: None,
        }],
        ..AppliedPatchSet::new()
    }
}

/// The errors for what a write couldn't keep: instances that didn't survive
/// the round trip through their files, and references that leave them.
fn unsaved_errors(unstored: Vec<Ref>, outside: Vec<(Ref, Ustr)>) -> Vec<WriteError> {
    let instances = unstored.into_iter().map(|id| WriteError {
        id,
        property: None,
        message: "This instance could not be stored on disk".into(),
    });
    let references = outside.into_iter().map(|(id, property)| WriteError {
        id,
        property: Some(property.to_string()),
        message: "This reference points at an instance in another file that has no Rojo_Id, \
                  so it was not saved"
            .into(),
    });
    instances.chain(references).collect()
}

/// Points `Ref` properties at the copies in `dom` of the tree instances they
/// named, where `copies` maps each copy to the tree instance it came from.
/// References to instances outside the copy are left as they are.
fn retarget_refs(dom: &mut WeakDom, copies: &HashMap<Ref, Ref>) {
    let to_copy: HashMap<Ref, Ref> = copies
        .iter()
        .map(|(copy, original)| (*original, *copy))
        .collect();
    for &copy in copies.keys() {
        let Some(instance) = dom.get_by_ref_mut(copy) else {
            continue;
        };
        for value in instance.properties.values_mut() {
            if let Variant::Ref(target) = value {
                if let Some(copied) = to_copy.get(target) {
                    *target = *copied;
                }
            }
        }
    }
}

/// Returns the directory a project node's `$path` names, resolved against the
/// folder of the project file that declares it, or `None` without a `$path`.
fn project_node_directory(
    project_file: &Path,
    node: &crate::project::ProjectNode,
) -> Option<PathBuf> {
    let mapping = node.path.as_ref()?;
    let folder = project_file.parent().unwrap_or(Path::new(""));
    Some(folder.join(mapping.path()))
}

/// Collects every id in a created subtree, root first.
fn collect_added_ids(ids: &AddedIds, out: &mut Vec<Ref>) {
    out.push(ids.id);
    for child in &ids.children {
        collect_added_ids(child, out);
    }
}

/// Pairs the ids Studio gave a created subtree with the instances read back
/// from disk. The roots always pair. Below them, children pair by name and
/// class, and same-named siblings pair in order. Returns the referents to
/// insert the snapshot with, and the ids of Studio's instances that have no
/// counterpart on disk, with all of their descendants.
fn pair_added_ids(snapshot: &InstanceSnapshot, ids: &AddedIds) -> (SnapshotReferents, Vec<Ref>) {
    let mut unpaired = Vec::new();
    let referents = pair_added_ids_inner(snapshot, ids, &mut unpaired);
    (referents, unpaired)
}

fn pair_added_ids_inner(
    snapshot: &InstanceSnapshot,
    ids: &AddedIds,
    unpaired: &mut Vec<Ref>,
) -> SnapshotReferents {
    let mut remaining: Vec<Option<&AddedIds>> = ids.children.iter().map(Some).collect();
    let children = snapshot
        .children
        .iter()
        .map(|child| {
            let found = remaining.iter_mut().find(|candidate| {
                candidate.is_some_and(|candidate| {
                    candidate.name == child.name && candidate.class_name == child.class_name
                })
            });
            match found.and_then(Option::take) {
                Some(child_ids) => pair_added_ids_inner(child, child_ids, unpaired),
                None => SnapshotReferents::default(),
            }
        })
        .collect();
    for leftover in remaining.into_iter().flatten() {
        collect_added_ids(leftover, unpaired);
    }
    SnapshotReferents {
        id: Some(ids.id),
        children,
    }
}

fn empty_update(id: Ref) -> PatchUpdate {
    PatchUpdate {
        id,
        changed_parent: None,
        changed_name: None,
        changed_class_name: None,
        changed_metadata: None,
        changed_properties: UstrMap::new(),
    }
}

/// What to do when a file changes between reading it and writing an edit.
#[derive(Debug, PartialEq, Eq)]
enum ChangedDuringWrite {
    /// Restore the new disk state in Studio, dropping the edit.
    Restore,
    /// Read the file again and redo the edit on top of it.
    Retry,
    /// Restore the disk state and report the edit as failed.
    GiveUp,
}

fn changed_during_write(authority: WriteAuthority, attempt: u32) -> ChangedDuringWrite {
    match authority {
        WriteAuthority::Filesystem => ChangedDuringWrite::Restore,
        WriteAuthority::Studio if attempt < STUDIO_WRITE_ATTEMPTS => ChangedDuringWrite::Retry,
        WriteAuthority::Studio => ChangedDuringWrite::GiveUp,
    }
}

/// What a batch of Studio writes has produced so far.
struct WriteOutcome<'a> {
    accepted: &'a mut PatchSet,
    conflicts: &'a mut PatchSet,
    errors: &'a mut Vec<WriteError>,
    messages: &'a mut Vec<AppliedPatchSet>,
}

/// What every write in a batch is checked against.
struct WriteContext<'a> {
    cursor: Option<u32>,
    /// Whether the batch began before the server's latest revision.
    stale: bool,
    authority: WriteAuthority,
    /// Every update in the batch, so catching up for one never sends Studio a
    /// disk value that a later update in the same batch replaces.
    batch: &'a [PatchUpdate],
}

fn file_payload_property(class: &str, middleware: Option<Middleware>) -> Option<Ustr> {
    match middleware {
        Some(Middleware::Text) => Some(ustr("Value")),
        Some(Middleware::Csv | Middleware::CsvDir) => Some(ustr("Contents")),
        _ if matches!(class, "Script" | "LocalScript" | "ModuleScript") => Some(ustr("Source")),
        _ => None,
    }
}

fn is_ordinary_property_update(
    update: &PatchUpdate,
    class: &str,
    middleware: Option<Middleware>,
) -> bool {
    let payload = file_payload_property(class, middleware);
    update.changed_name.is_none()
        && update.changed_parent.is_none()
        && update.changed_class_name.is_none()
        && update.changed_metadata.is_none()
        && !update.changed_properties.is_empty()
        && update
            .changed_properties
            .iter()
            .all(|(key, value)| Some(*key) != payload && !matches!(value, Some(Variant::Ref(_))))
}

fn removed_on_disk(id: Ref) -> WriteError {
    WriteError {
        id,
        property: None,
        message: "This instance was removed on disk, and two-way sync can't recreate it".into(),
    }
}

/// Splits disk changes into the ones to send to Studio and the ones a pending
/// Studio edit is about to replace, so Studio isn't sent a value it will
/// immediately overwrite. A disk value equal to Studio's is sent normally,
/// since nothing will be written for it and other clients still need it.
fn split_studio_edits(mut disk_changes: PatchSet, batch: &[PatchUpdate]) -> (PatchSet, PatchSet) {
    let mut overwritten = PatchSet::new();
    for entry in &mut disk_changes.updated_instances {
        let Some(update) = batch.iter().find(|update| update.id == entry.id) else {
            continue;
        };
        let mut held = PatchUpdate {
            id: entry.id,
            changed_name: None,
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::new(),
            changed_metadata: None,
        };
        if update.changed_name.is_some() && entry.changed_name != update.changed_name {
            held.changed_name = entry.changed_name.take();
        }
        for (key, studio_value) in &update.changed_properties {
            let differs = entry
                .changed_properties
                .get(key)
                .is_some_and(|disk_value| !same_value(disk_value.as_ref(), studio_value.as_ref()));
            if differs {
                let disk_value = entry.changed_properties.remove(key).unwrap();
                held.changed_properties.insert(*key, disk_value);
            }
        }
        if held.changed_name.is_some() || !held.changed_properties.is_empty() {
            overwritten.updated_instances.push(held);
        }
    }
    disk_changes.updated_instances.retain(|entry| {
        entry.changed_name.is_some()
            || entry.changed_class_name.is_some()
            || !entry.changed_properties.is_empty()
            || entry.changed_metadata.is_some()
    });
    (disk_changes, overwritten)
}

/// Brings the server's copy up to date with disk changes it hasn't applied
/// yet, sending them to Studio except for values Studio is about to overwrite.
fn catch_up(
    tree: &mut RojoTree,
    disk_changes: PatchSet,
    batch: &[PatchUpdate],
    messages: &mut Vec<AppliedPatchSet>,
) {
    let (to_studio, overwritten) = split_studio_edits(disk_changes, batch);
    let applied = apply_patch_set(tree, to_studio);
    if !applied.is_empty() {
        messages.push(applied);
    }
    apply_patch_set(tree, overwritten);
}

/// Returns whether the disk changed between two reads of the same source.
fn snapshots_differ(baseline: Option<InstanceSnapshot>, latest: Option<&InstanceSnapshot>) -> bool {
    match (baseline, latest) {
        (Some(baseline), Some(latest)) => {
            // Patch application is what resolves attribute IDs and references, so
            // the baseline is built the way the server builds its own tree.
            let mut baseline_tree = RojoTree::new(InstanceSnapshot::new());
            let root = baseline_tree.get_root_id();
            let patch = compute_patch_set(Some(baseline), &baseline_tree, root);
            apply_patch_set(&mut baseline_tree, patch);
            compute_patch_set(
                Some(latest.clone()),
                &baseline_tree,
                baseline_tree.get_root_id(),
            ) != PatchSet::new()
        }
        (None, None) => false,
        _ => true,
    }
}

/// What became of a Studio deletion of an instance stored inline.
enum InlineRemoval {
    Removed,
    /// The owner's disk state wins, so this snapshot of it is restored.
    Restore(Box<Option<InstanceSnapshot>>),
}

/// A Studio write to an instance stored in its owner's `init.meta.json`.
enum InlineEdit<'a> {
    Update {
        id: Ref,
        name: &'a str,
        properties: &'a UstrMap<Variant>,
    },
    Remove(Ref),
}

/// The single file that stores an instance which has no file of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredIn {
    /// The `init.meta.json` of this directory, among its inline children.
    Inline(Ref),
    /// This model file, which stores its whole subtree.
    Model(Ref),
}

impl StoredIn {
    fn owner(self) -> Ref {
        match self {
            StoredIn::Inline(owner) | StoredIn::Model(owner) => owner,
        }
    }
}

fn is_model_middleware(middleware: Option<Middleware>) -> bool {
    matches!(
        middleware,
        Some(Middleware::Rbxm | Middleware::Rbxmx | Middleware::JsonModel)
    )
}

/// Returns the file that stores `id`'s children, when they can't have files of
/// their own: an instance stored inline keeps its children in the same
/// `init.meta.json`, and a model file or anything inside one keeps them in the
/// model. Returns `None` when children get files of their own.
fn children_stored_in(tree: &RojoTree, id: Ref) -> Option<StoredIn> {
    let metadata = tree.get_metadata(id)?;
    if is_model_middleware(metadata.middleware) {
        return Some(StoredIn::Model(id));
    }
    if metadata.instigating_source.is_some() {
        return None;
    }
    let mut current = tree.get_instance(id)?.parent();
    while let Some(ancestor) = tree.get_instance(current) {
        let metadata = ancestor.metadata();
        match metadata.instigating_source {
            Some(InstigatingSource::Path(_)) if is_model_middleware(metadata.middleware) => {
                return Some(StoredIn::Model(current));
            }
            Some(InstigatingSource::Path(_)) if metadata.middleware == Some(Middleware::Dir) => {
                return Some(StoredIn::Inline(current));
            }
            Some(_) => return None,
            None => current = ancestor.parent(),
        }
    }
    None
}

/// Returns the file that stores `id` itself, when it has no file of its own:
/// the nearest ancestor with a source, if that's a directory or a model file.
fn stored_in(tree: &RojoTree, id: Ref) -> Option<StoredIn> {
    if tree.get_metadata(id)?.instigating_source.is_some() {
        return None;
    }
    let mut current = tree.get_instance(id)?.parent();
    while let Some(ancestor) = tree.get_instance(current) {
        let metadata = ancestor.metadata();
        match metadata.instigating_source {
            Some(InstigatingSource::Path(_)) if is_model_middleware(metadata.middleware) => {
                return Some(StoredIn::Model(current));
            }
            Some(InstigatingSource::Path(_)) if metadata.middleware == Some(Middleware::Dir) => {
                return Some(StoredIn::Inline(current));
            }
            Some(_) => return None,
            None => current = ancestor.parent(),
        }
    }
    None
}

/// A copy of the instances one file stores, with the tree id each copy came
/// from, so the file can be rewritten whole after an edit.
struct StoredCopy {
    dom: WeakDom,
    /// Which tree instance each copy came from, or which id a new one takes.
    ids: HashMap<Ref, Ref>,
}

impl StoredCopy {
    /// Copies what `stored` holds: an inline owner with the instances its
    /// metadata stores, or a model file with its whole subtree.
    fn new(tree: &RojoTree, stored: StoredIn) -> Self {
        let owner = stored.owner();
        let builder = |id: Ref| {
            let instance = tree.get_instance(id).unwrap();
            InstanceBuilder::new(instance.class_name())
                .with_name(instance.name())
                .with_properties(properties_with_id(&instance))
        };
        let mut dom = WeakDom::new(builder(owner));
        let mut ids = HashMap::from([(dom.root_ref(), owner)]);
        let mut pending = vec![(owner, dom.root_ref())];
        while let Some((source, destination)) = pending.pop() {
            for &child in tree.get_instance(source).unwrap().children() {
                if source == owner && stored_in(tree, child) != Some(stored) {
                    continue;
                }
                let copy = dom.insert(destination, builder(child));
                ids.insert(copy, child);
                pending.push((child, copy));
            }
        }
        retarget_refs(&mut dom, &ids);
        StoredCopy { dom, ids }
    }

    /// Takes a copy and its descendants out, for a move to another file.
    /// References to them point at the tree instances again, since those
    /// still exist elsewhere.
    fn move_out(&mut self, copy: Ref) {
        let moved: HashMap<Ref, Ref> = self
            .dom
            .descendants_of(copy)
            .filter_map(|instance| {
                let referent = instance.referent();
                self.ids
                    .get(&referent)
                    .map(|original| (referent, *original))
            })
            .collect();
        self.dom.destroy(copy);
        for &remaining in self.ids.keys() {
            let Some(instance) = self.dom.get_by_ref_mut(remaining) else {
                continue;
            };
            for value in instance.properties.values_mut() {
                if let Variant::Ref(target) = value {
                    if let Some(original) = moved.get(target) {
                        *target = *original;
                    }
                }
            }
        }
    }

    /// Deletes a copy and its descendants. References to them become nil, the
    /// way they would for an instance that no longer exists.
    fn delete(&mut self, copy: Ref) {
        let deleted: HashSet<Ref> = self
            .dom
            .descendants_of(copy)
            .map(|instance| instance.referent())
            .collect();
        self.dom.destroy(copy);
        for &remaining in self.ids.keys() {
            let Some(instance) = self.dom.get_by_ref_mut(remaining) else {
                continue;
            };
            for value in instance.properties.values_mut() {
                if let Variant::Ref(target) = value {
                    if deleted.contains(target) {
                        *target = Ref::none();
                    }
                }
            }
        }
    }

    /// Returns the copy of a tree instance.
    fn copy_of(&self, id: Ref) -> Option<Ref> {
        self.ids
            .iter()
            .find_map(|(copy, original)| (*original == id).then_some(*copy))
    }

    /// Moves `model`'s top-level instance under `parent`'s copy, recording the
    /// ids `added` gives it.
    fn add(&mut self, parent: Ref, mut model: WeakDom, added: &AddedIds) -> anyhow::Result<()> {
        let destination = self
            .copy_of(parent)
            .context("The parent isn't stored in this file")?;
        let top = match model.root().children() {
            [top] => *top,
            other => anyhow::bail!(
                "A created subtree must have exactly one root, not {}",
                other.len()
            ),
        };
        let mut pending = vec![(top, added)];
        while let Some((instance, ids)) = pending.pop() {
            self.ids.insert(instance, ids.id);
            let children = model.get_by_ref(instance).unwrap().children().to_vec();
            let mut remaining: Vec<Option<&AddedIds>> = ids.children.iter().map(Some).collect();
            for child in children {
                let child_instance = model.get_by_ref(child).unwrap();
                let found = remaining.iter_mut().find(|candidate| {
                    candidate.is_some_and(|candidate| {
                        candidate.name == child_instance.name
                            && candidate.class_name == child_instance.class
                    })
                });
                if let Some(child_ids) = found.and_then(Option::take) {
                    pending.push((child, child_ids));
                }
            }
        }
        // Copy referents survive a transfer, so the recorded ids stay valid.
        model.transfer(top, &mut self.dom, destination);
        // A moved instance can point at one the file already holds, and the
        // reverse, so both now point at copies.
        retarget_refs(&mut self.dom, &self.ids);
        Ok(())
    }

    /// Returns the ids this copy will hold once written, shaped like the copy
    /// below `root`. Copies without an id are left out with their descendants.
    fn added_ids(&self, root: Ref) -> AddedIds {
        let instance = self.dom.get_by_ref(root).unwrap();
        AddedIds {
            id: self.ids.get(&root).copied().unwrap_or_else(Ref::none),
            name: instance.name.clone(),
            class_name: instance.class,
            children: instance
                .children()
                .iter()
                .filter(|child| self.ids.contains_key(child))
                .map(|child| self.added_ids(*child))
                .collect(),
        }
    }
}

/// Returns the directory whose `init.meta.json` stores this instance, if it is
/// stored inline. Descendants of model files also lack sources of their own,
/// but their nearest sourced ancestor is the model file, not a directory.
fn inline_owner(tree: &RojoTree, id: Ref) -> Option<Ref> {
    if tree.get_metadata(id)?.instigating_source.is_some() {
        return None;
    }
    let mut current = tree.get_instance(id)?.parent();
    while let Some(ancestor) = tree.get_instance(current) {
        match ancestor.metadata().instigating_source {
            Some(InstigatingSource::Path(_)) => {
                return (ancestor.metadata().middleware == Some(Middleware::Dir))
                    .then_some(current);
            }
            Some(_) => return None,
            None => current = ancestor.parent(),
        }
    }
    None
}

/// Returns whether a directory stores any of its children inline.
fn has_inline_children(tree: &RojoTree, id: Ref) -> bool {
    tree.get_instance(id).is_some_and(|instance| {
        instance.metadata().middleware == Some(Middleware::Dir)
            && instance
                .children()
                .iter()
                .any(|child| inline_owner(tree, *child) == Some(id))
    })
}

/// Copies an owner directory and the instances its `init.meta.json` stores,
/// with one Studio edit applied, so the file can be rewritten whole. Children
/// that have files of their own are left out and keep those files.
fn inline_subtree(tree: &RojoTree, owner: Ref, edit: &InlineEdit) -> WeakDom {
    let builder = |id: Ref| {
        let instance = tree.get_instance(id).unwrap();
        let (name, properties) = match edit {
            InlineEdit::Update {
                id: target,
                name,
                properties,
            } if *target == id => (*name, (*properties).clone()),
            _ => (instance.name(), instance.properties().clone()),
        };
        InstanceBuilder::new(instance.class_name())
            .with_name(name)
            .with_properties(properties)
    };
    let mut dom = WeakDom::new(builder(owner));
    let mut pending = vec![(owner, dom.root_ref())];
    while let Some((source, destination)) = pending.pop() {
        for &child in tree.get_instance(source).unwrap().children() {
            let removed = matches!(edit, InlineEdit::Remove(target) if *target == child);
            if removed || (source == owner && inline_owner(tree, child) != Some(owner)) {
                continue;
            }
            let copy = dom.insert(destination, builder(child));
            pending.push((child, copy));
        }
    }
    dom
}

/// Copies a subtree out of the tree, metadata and referents included, so it
/// can be inserted again exactly as it was.
fn backup_subtree(tree: &RojoTree, id: Ref) -> (InstanceSnapshot, SnapshotReferents) {
    let instance = tree.get_instance(id).unwrap();
    let (children, referents): (Vec<_>, Vec<_>) = instance
        .children()
        .iter()
        .map(|child| backup_subtree(tree, *child))
        .unzip();
    (
        InstanceSnapshot::new()
            .name(instance.name())
            .class_name(instance.class_name())
            .properties(instance.properties().clone())
            .metadata(instance.metadata().clone())
            .children(children),
        SnapshotReferents {
            id: Some(id),
            children: referents,
        },
    )
}

/// Builds a snapshot of an instance and its descendants as the server has them.
fn snapshot_from_tree(tree: &RojoTree, id: Ref) -> InstanceSnapshot {
    let instance = tree.get_instance(id).unwrap();
    InstanceSnapshot::new()
        .name(instance.name())
        .class_name(instance.class_name())
        .properties(instance.properties().clone())
        .children(
            instance
                .children()
                .iter()
                .map(|child| snapshot_from_tree(tree, *child))
                .collect::<Vec<_>>(),
        )
}

/// Restores the disk state for a rejected write, comparing the owner's whole
/// file when the instance is stored inline. `latest` is a shallow read, so
/// children stored in their own files are left as they are.
fn disk_restoration(
    tree: &RojoTree,
    owner: Ref,
    update: &PatchUpdate,
    latest: Option<InstanceSnapshot>,
) -> PatchSet {
    if owner == update.id {
        return filesystem_restoration(tree, update, latest, true);
    }
    let mut patch = compute_shallow_patch_set(latest, tree, owner);
    let removed: HashSet<Ref> = patch.removed_instances.iter().copied().collect();
    let mut current = update.id;
    while current != owner {
        if removed.contains(&current) {
            // The instance no longer exists on disk, so its removal restores it.
            return patch;
        }
        current = match tree.get_instance(current) {
            Some(instance) => instance.parent(),
            None => return patch,
        };
    }
    if removed.contains(&owner) {
        return patch;
    }

    // Values the disk patch leaves alone already match the disk on the server,
    // but Studio may hold its own edit of them, so they are sent back too.
    let instance = tree.get_instance(update.id).unwrap();
    let existing = patch
        .updated_instances
        .iter()
        .position(|entry| entry.id == update.id);
    let mut restoration = existing
        .map(|index| patch.updated_instances.remove(index))
        .unwrap_or_else(|| PatchUpdate {
            id: update.id,
            changed_name: None,
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::new(),
            changed_metadata: None,
        });
    for key in update.changed_properties.keys() {
        restoration
            .changed_properties
            .entry(*key)
            .or_insert_with(|| instance.properties().get(key).cloned());
    }
    if update.changed_name.is_some() && restoration.changed_name.is_none() {
        restoration.changed_name = Some(instance.name().to_owned());
    }
    if update.changed_class_name.is_some() && restoration.changed_class_name.is_none() {
        restoration.changed_class_name = Some(instance.class_name());
    }
    if update.changed_metadata.is_some() && restoration.changed_metadata.is_none() {
        restoration.changed_metadata = Some(instance.metadata().clone());
    }
    patch.updated_instances.push(restoration);
    patch
}

fn filesystem_restoration(
    tree: &RojoTree,
    update: &PatchUpdate,
    latest: Option<crate::snapshot::InstanceSnapshot>,
    shallow: bool,
) -> PatchSet {
    let mut patch = if shallow {
        compute_shallow_patch_set(latest.clone(), tree, update.id)
    } else {
        compute_patch_set(latest.clone(), tree, update.id)
    };
    if let Some(latest) = latest {
        // The requested properties may still match the server but differ in Studio.
        // Merge their restoration into the real-ID patch without duplicate updates.
        let existing = patch
            .updated_instances
            .iter()
            .position(|entry| entry.id == update.id);
        let mut restoration = existing
            .map(|index| patch.updated_instances.remove(index))
            .unwrap_or_else(|| PatchUpdate {
                id: update.id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            });
        restoration.changed_properties.extend(
            update
                .changed_properties
                .keys()
                .map(|key| (*key, latest.properties.get(key).cloned())),
        );
        if update.changed_parent.is_some() {
            restoration.changed_parent = tree
                .get_instance(update.id)
                .map(|instance| instance.parent());
        }
        if update.changed_name.is_some() {
            restoration.changed_name = Some(latest.name.to_string());
        }
        if update.changed_class_name.is_some() {
            restoration.changed_class_name = Some(latest.class_name);
        }
        if update.changed_metadata.is_some() {
            restoration.changed_metadata = Some(latest.metadata);
        }
        patch.updated_instances.push(restoration);
    }
    patch
}

/// Returns the adjacent metadata of the file at `path`, which `id` comes from,
/// that goes with the file when it's deleted or moved. Without it, an instance
/// written at the same name later would pick up its properties and ID. Files
/// that share a stem, like `Code.luau` and `Code.txt`, share it too, so it
/// stays while another instance still reads it.
fn own_sidecars(tree: &RojoTree, id: Ref, path: &Path) -> Vec<PathBuf> {
    let Some(metadata) = tree.get_metadata(id) else {
        return Vec::new();
    };
    metadata
        .relevant_paths
        .iter()
        .filter(|sidecar| {
            sidecar.as_path() != path
                && sidecar.parent() == path.parent()
                && sidecar
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.ends_with(".meta.json") || name.ends_with(".meta.jsonc")
                    })
                && tree
                    .get_ids_at_path(sidecar)
                    .iter()
                    .all(|holder| *holder == id)
        })
        .cloned()
        .collect()
}

/// Whether a file named `name` changes the folder it's in, or what the folder's
/// scripts resolve to, like the aliases in `.luaurc`, rather than only adding
/// a child. The folder is read again for these.
fn changes_folder(name: &str) -> bool {
    name.starts_with("init.")
        || name == ".luaurc"
        || name.ends_with(".meta.json")
        || name.ends_with(".meta.jsonc")
        || name.ends_with(".project.json")
        || name.ends_with(".project.jsonc")
}

/// The file syncback writes to keep an empty directory in version control.
const EMPTY_DIR_KEEP_NAME: &str = ".gitkeep";

fn is_keep_file(path: &Path) -> bool {
    path.file_name() == Some(EMPTY_DIR_KEEP_NAME.as_ref())
}

/// Compare authoritative source roots, not installed package names or DOM mounts.
/// Inline instances inherit ownership from their nearest file-backed ancestor.
fn package_reparent_block(tree: &RojoTree, id: Ref, parent: Ref) -> Option<String> {
    let packages = tree.root().metadata().context.package_sources.as_ref()?;
    let owner = |mut id: Ref| {
        while let Some(instance) = tree.get_instance(id) {
            let metadata = instance.metadata();
            let path = match &metadata.instigating_source {
                Some(InstigatingSource::Path(path)) => Some(path.as_path()),
                Some(InstigatingSource::ProjectNode { node, .. }) => {
                    node.path.as_ref().map(|path| path.path())
                }
                None => {
                    id = instance.parent();
                    continue;
                }
            };
            if let Some(path) = path {
                return packages.local_source_owner(&packages.original_path(path));
            }
            if let Some((source, _)) = &metadata.context.package_mount {
                return Some(source.clone());
            }
            id = instance.parent();
        }
        None
    };
    let source = owner(id);
    let destination = owner(parent);
    if source != destination && (source.is_some() || destination.is_some()) {
        Some("Reparenting crosses authoritative local package source ownership. Package descendants may only move within the same local source".into())
    } else {
        None
    }
}

fn package_write_block(
    tree: &RojoTree,
    vfs: &Vfs,
    id: Ref,
    structural: bool,
    stale: bool,
) -> Option<String> {
    let packages = tree.root().metadata().context.package_sources.as_ref()?;
    let generation_changed = crate::package_sources::PackageSources::load(vfs, &packages.root)
        .ok()
        .flatten()
        .as_ref()
        != Some(packages.as_ref());
    let inspect = |id: Ref| {
        let mut current = Some(id);
        while let Some(instance) = current.and_then(|id| tree.get_instance(id)) {
            let metadata = instance.metadata();
            if structural && metadata.is_package_mount_root() {
                return Some("Local package mounts are declared by wally.toml. Change the declaration explicitly instead of moving, renaming, or deleting the mount".into());
            }
            for path in &metadata.relevant_paths {
                if let Some(reason) = packages.blocked(vfs, path, false) {
                    return Some(reason);
                }
            }
            if metadata.context.package_mount.is_some() && stale {
                return Some(
                    "Package source baseline is stale. Filesystem and Studio edits were preserved"
                        .into(),
                );
            }
            match crate::syncback::package_source_conflict(vfs, tree, instance.id()) {
                Ok(false) => {},
                Ok(true) => return Some("Package source changed concurrently. Filesystem and Studio edits were preserved".into()),
                Err(error) => return Some(format!("Cannot validate package source. {error:#}")),
            }
            if metadata.context.package_mount.is_some() && generation_changed {
                return Some(
                    "Wally package ownership changed. Restart Rojo after reinstalling".into(),
                );
            }
            if metadata.instigating_source.is_some() {
                break;
            }
            current = Some(instance.parent());
        }
        None
    };
    if let Some(reason) = inspect(id) {
        return Some(reason);
    }
    if structural {
        for descendant in tree.descendants(id) {
            if let Some(reason) = inspect(descendant.id()) {
                return Some(reason);
            }
        }
    }
    None
}

fn canonicalize_or_original(vfs: &Vfs, path: PathBuf) -> PathBuf {
    match vfs.canonicalize(&path) {
        Ok(canonical) => canonical,
        // Deleting a directory reports its contents after the directory itself
        // is gone, so a missing path is expected.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => path,
        Err(err) => {
            log::warn!("Could not canonicalize {}: {err}", path.display());
            path
        }
    }
}

fn requires_mapping_transaction(
    tree: &RojoTree,
    patches: &[PatchSet],
    source_count: usize,
) -> bool {
    // Multi-mount diffs are recomputed sequentially. A file can change again during
    // those reads, so preflight property-only diffs cannot waive mapping validation.
    !patches.is_empty()
        && (source_count > 1
            || patches
                .iter()
                .any(|patch| changes_require_context(tree, patch)))
}

fn changes_require_context(tree: &RojoTree, patch: &PatchSet) -> bool {
    !patch.added_instances.is_empty()
        || !patch.removed_instances.is_empty()
        || patch.updated_instances.iter().any(|update| {
            update.changed_name.is_some()
                || update.changed_class_name.is_some()
                || update.changed_parent.is_some()
                || update.changed_metadata.as_ref().is_some_and(|metadata| {
                    let previous = tree.get_metadata(update.id).unwrap();
                    metadata.relevant_paths != previous.relevant_paths
                        || metadata.installed_paths != previous.installed_paths
                        || metadata.context != previous.context
                })
        })
}

fn compute_and_apply_changes(
    tree: &mut RojoTree,
    vfs: &Vfs,
    id: Ref,
    shallow: bool,
) -> Option<AppliedPatchSet> {
    compute_changes(tree, vfs, id, shallow).map(|patch| apply_patch_set(tree, patch))
}

/// Re-reads only the affected source. A shallow refresh leaves independently
/// stored children to their own events. Preparing a diff must not mutate the tree,
/// so mapping validation can still fail without exposing a partial revision.
#[profiling::function]
fn compute_changes(tree: &RojoTree, vfs: &Vfs, id: Ref, shallow: bool) -> Option<PatchSet> {
    let metadata = tree
        .get_metadata(id)
        .expect("metadata missing for instance present in tree");

    let instigating_source = match &metadata.instigating_source {
        Some(path) => path,
        None => {
            log::error!(
                "Instance {:?} did not have an instigating source, but was considered for an update.",
                id
            );
            log::error!("This is a bug. Please file an issue!");
            return None;
        }
    };

    // How we process a file change event depends on what created this
    // file/folder in the first place.
    let patch_set = match instigating_source {
        InstigatingSource::Path(path) => match vfs.metadata(path).with_not_found() {
            Ok(Some(_)) => {
                // Our instance was previously created from a path and that
                // path still exists. We can generate a snapshot starting at
                // that path and use it as the source for our patch.

                let snapshot = if shallow {
                    snapshot_shallow_from_vfs(&metadata.context, vfs, path)
                } else {
                    snapshot_from_vfs(&metadata.context, vfs, path)
                };
                let snapshot = match snapshot {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        log::error!("Snapshot error: {:?}", err);
                        return None;
                    }
                };

                if shallow {
                    compute_shallow_patch_set(snapshot, tree, id)
                } else {
                    compute_patch_set(snapshot, tree, id)
                }
            }
            Ok(None) => {
                // Our instance was previously created from a path, but that
                // path no longer exists.
                //
                // We associate deleting the instigating file for an
                // instance with deleting that instance.

                let mut patch_set = PatchSet::new();
                patch_set.removed_instances.push(id);
                patch_set
            }
            Err(err) => {
                log::error!("Error processing filesystem change: {:?}", err);
                return None;
            }
        },

        InstigatingSource::ProjectNode {
            path,
            name,
            node,
            parent_class,
        } => {
            // This instance is the direct subject of a project node. Since
            // there might be information associated with our instance from
            // the project file, we snapshot the entire project node again.

            let snapshot_result = snapshot_project_node(
                &metadata.context,
                path,
                name,
                node,
                vfs,
                parent_class.as_ref().map(|name| name.as_str()),
            );

            let snapshot = match snapshot_result {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    log::error!("{:?}", err);
                    return None;
                }
            };

            compute_patch_set(snapshot, tree, id)
        }
    };

    Some(patch_set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{snapshot::InstanceMetadata, variant_eq::variant_eq};
    use memofs::{InMemoryFs, VfsSnapshot};
    use rbx_dom_weak::{
        types::{Attributes, CFrame, Matrix3, NumberRange, Vector3},
        ustr,
    };
    use std::path::Path;

    fn added(name: &str, class: &str, children: Vec<AddedIds>) -> AddedIds {
        AddedIds {
            id: Ref::new(),
            name: name.into(),
            class_name: ustr(class),
            children,
        }
    }

    fn snapshot(
        name: &'static str,
        class: &str,
        children: Vec<InstanceSnapshot>,
    ) -> InstanceSnapshot {
        InstanceSnapshot::new()
            .name(name)
            .class_name(class)
            .children(children)
    }

    // Every Studio id pairs with the disk instance of the same name and class,
    // same-named siblings pair in order, and an id with no counterpart on disk
    // is reported along with its descendants.
    #[test]
    fn pairs_created_ids_with_the_instances_read_back() {
        let first = added("Same", "Folder", Vec::new());
        let second = added("Same", "Folder", Vec::new());
        let script = added("Code", "ModuleScript", Vec::new());
        let dropped_child = added("Inner", "Folder", Vec::new());
        let dropped = added("Ignored", "Folder", vec![dropped_child.clone()]);
        let root = added(
            "Root",
            "Folder",
            vec![
                first.clone(),
                script.clone(),
                dropped.clone(),
                second.clone(),
            ],
        );
        let disk = snapshot(
            "Root",
            "Folder",
            vec![
                snapshot("Code", "ModuleScript", Vec::new()),
                snapshot("Same", "Folder", Vec::new()),
                snapshot("Same", "Folder", Vec::new()),
                snapshot("OnlyOnDisk", "Folder", Vec::new()),
            ],
        );

        let (referents, unpaired) = pair_added_ids(&disk, &root);
        assert_eq!(referents.id, Some(root.id));
        let ids: Vec<_> = referents.children.iter().map(|child| child.id).collect();
        assert_eq!(
            ids,
            [Some(script.id), Some(first.id), Some(second.id), None]
        );
        assert_eq!(unpaired, [dropped.id, dropped_child.id]);

        // Pairing is pure.
        assert_eq!(pair_added_ids(&disk, &root), (referents, unpaired));
    }

    #[test]
    fn a_class_mismatch_does_not_pair() {
        let child = added("Value", "StringValue", Vec::new());
        let root = added("Root", "Folder", vec![child.clone()]);
        let disk = snapshot(
            "Root",
            "Folder",
            vec![snapshot("Value", "Folder", Vec::new())],
        );
        let (referents, unpaired) = pair_added_ids(&disk, &root);
        assert_eq!(referents.children[0].id, None);
        assert_eq!(unpaired, [child.id]);
    }

    #[test]
    fn resolves_a_project_node_path_against_its_project_file() {
        let node: crate::project::ProjectNode =
            serde_json::from_str(r#"{"$path": "src/shared"}"#).unwrap();
        assert_eq!(
            project_node_directory(Path::new("/project/default.project.json"), &node),
            Some(PathBuf::from("/project/src/shared"))
        );
        let pathless: crate::project::ProjectNode =
            serde_json::from_str(r#"{"$className": "Folder"}"#).unwrap();
        assert_eq!(
            project_node_directory(Path::new("/project/default.project.json"), &pathless),
            None
        );
    }

    /// A job serving the project file `/project/{file}`, read the way the
    /// server reads it at startup.
    fn project_job(file: &str, project: &str, files: Vec<(&str, VfsSnapshot)>) -> JobThreadContext {
        let mut entries = files;
        entries.push((file, VfsSnapshot::file(project)));
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/project", VfsSnapshot::dir(entries))
            .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let path = PathBuf::from("/project").join(file);
        let project = Project::load_exact(&vfs, &path, Some("Fallback")).unwrap();
        let root = snapshot_from_vfs(&Default::default(), &vfs, &path)
            .unwrap()
            .unwrap();
        // Under a stand-in root, like `script_job`, which `find` expects.
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![root]));
        JobThreadContext {
            tree: Arc::new(Mutex::new(tree)),
            vfs,
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        }
    }

    /// A relayout never moves files another instance also comes from, like a
    /// folder two project nodes both map, and leaves them where they are.
    #[test]
    fn relayouts_refuse_files_another_instance_shares() {
        // The folder itself, or only a file inside it.
        for second in [r#"{"$path":"src"}"#, r#"{"$path":"src/Mover/Code.luau"}"#] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::create_dir_all(root.join("src/Mover")).unwrap();
            std::fs::write(root.join("src/Mover/Code.luau"), "return 1").unwrap();
            std::fs::write(
                root.join("default.project.json"),
                format!(
                    r#"{{"name":"Test","tree":{{"$className":"Folder","First":{{"$path":"src"}},"Second":{second}}}}}"#
                ),
            )
            .unwrap();
            let job = disk_job(root);
            let mover = find(&job, &["First", "Mover"]);
            let mut tree = job.tree.lock().unwrap();
            let error = job.relayout_instance(&mut tree, mover).unwrap_err();
            assert!(error.to_string().contains("another instance"), "{error:#}");
            drop(tree);
            assert!(root.join("src/Mover/Code.luau").is_file(), "{second}");
            assert!(
                !crate::syncback::relocation::relayout_directory(&job.project())
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_some()),
                "{second} set something aside"
            );
        }
    }

    /// A job serving the `default.project.json` in `root` on the real file
    /// system, for tests that need its locks. Like a server, it serves from the
    /// path the VFS canonicalizes `root` to.
    fn disk_job(root: &Path) -> JobThreadContext {
        let vfs = Arc::new(Vfs::new(memofs::StdBackend::new().unwrap()));
        vfs.set_watch_enabled(false);
        let path = vfs.canonicalize(root).unwrap().join("default.project.json");
        let project = Project::load_exact(&vfs, &path, Some("Fallback")).unwrap();
        let snapshot = snapshot_from_vfs(&Default::default(), &vfs, &path)
            .unwrap()
            .unwrap();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .name("Root")
                .children(vec![snapshot]),
        );
        JobThreadContext {
            tree: Arc::new(Mutex::new(tree)),
            vfs,
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        }
    }

    // Use the real disk backend, but deliver project events synchronously. A
    // rejected relayout temporarily sets the model aside, so neither a sleep
    // nor polling for the original file proves that rollback has completed.
    #[test]
    fn model_tree_relayouts_never_touch_excluded_files_or_lose_instances() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let storage = root.join("src/ReplicatedStorage");
        std::fs::create_dir_all(storage.join("Mover/Dropped")).unwrap();
        std::fs::write(storage.join("Mover/Keep.luau"), "return 'keep'").unwrap();
        std::fs::write(storage.join("Mover/Dropped/Code.luau"), "return 1").unwrap();
        let project_path = root.join("default.project.json");
        let project = |rules: &str| {
            format!(
                r#"{{"name":"Test","tree":{{"$className":"DataModel","$path":"src"}},"syncbackRules":{{"instancesAsDirectories":true,{rules}}}}}"#
            )
        };
        std::fs::write(&project_path, project(r#""modelTrees":[]"#)).unwrap();
        let job = disk_job(root);
        job.note_authority(WriteAuthority::Studio);
        let mover = find(&job, &["ReplicatedStorage", "Mover"]);
        let keep = find(&job, &["ReplicatedStorage", "Mover", "Keep"]);
        let dropped = find(&job, &["ReplicatedStorage", "Mover", "Dropped"]);
        let code = find(&job, &["ReplicatedStorage", "Mover", "Dropped", "Code"]);
        let set_rules = |rules: &str| {
            std::fs::write(&project_path, project(rules)).unwrap();
            // Returning from the handler is the completion barrier for both
            // successful relayouts and rejected ones, including rollback.
            job.handle_vfs_event(VfsEvent::Write(
                job.vfs.canonicalize(&project_path).unwrap(),
            ));
            assert_eq!(
                job.project().syncback_rules,
                Project::load_exact(&job.vfs, &project_path, Some("Test"))
                    .unwrap()
                    .syncback_rules
            );
        };

        // disk_job puts the project beneath a stand-in root, so tree paths
        // include its name, unlike the project-rooted serve integration tests.
        set_rules(
            r#""modelTrees":[{"pattern":"Test/ReplicatedStorage/Mover"}],"ignorePaths":["src/ReplicatedStorage/Mover/Keep.luau"]"#,
        );
        assert_eq!(
            std::fs::read_to_string(storage.join("Mover/Keep.luau")).unwrap(),
            "return 'keep'"
        );
        assert!(!storage.join("Mover.rbxm").exists());

        set_rules(r#""modelTrees":[{"pattern":"Test/ReplicatedStorage/Mover"}]"#);
        let model_before = std::fs::read(storage.join("Mover.rbxm")).unwrap();
        assert!(!storage.join("Mover").exists());

        // Going back to directories would drop what ignoreTrees excludes.
        set_rules(r#""modelTrees":[],"ignoreTrees":["Test/ReplicatedStorage/Mover/Dropped"]"#);
        assert_eq!(
            std::fs::read(storage.join("Mover.rbxm")).unwrap(),
            model_before
        );
        assert!(!storage.join("Mover").exists());
        assert_eq!(find(&job, &["ReplicatedStorage", "Mover"]), mover);
        assert_eq!(find(&job, &["ReplicatedStorage", "Mover", "Keep"]), keep);
        assert_eq!(
            find(&job, &["ReplicatedStorage", "Mover", "Dropped"]),
            dropped
        );
        assert_eq!(
            find(&job, &["ReplicatedStorage", "Mover", "Dropped", "Code"]),
            code
        );
    }

    /// A relayout refused because its folder's metadata changed on disk
    /// leaves that change alone, since nothing was written.
    #[test]
    fn refused_relayouts_keep_an_editors_save() {
        let (job, _) = inline_job();
        let nested = find(&job, &["Nested"]);
        let edited = INLINE_META.replace("0.125", "0.75");
        job.vfs
            .write("/project/Props/init.meta.json", &edited)
            .unwrap();
        let mut tree = job.tree.lock().unwrap();
        assert!(job.relayout_instance(&mut tree, nested).is_err());
        drop(tree);
        assert_eq!(
            String::from_utf8(
                job.vfs
                    .read("/project/Props/init.meta.json")
                    .unwrap()
                    .to_vec()
            )
            .unwrap(),
            edited
        );
        find(&job, &["Nested", "Deep"]);
    }

    /// Only files that change their folder, or what its scripts resolve to,
    /// make the folder be read again. Anything else is read on its own.
    #[test]
    fn only_files_that_change_their_folder_reread_it() {
        for name in [
            "init.luau",
            "init.server.lua",
            "init.meta.json",
            "init.meta.jsonc",
            ".luaurc",
            "Code.meta.json",
            "Code.meta.jsonc",
            "default.project.json",
            "default.project.jsonc",
            "nested.project.json",
        ] {
            assert!(changes_folder(name), "{name}");
        }
        for name in [
            "Code.luau",
            "Folder",
            "Model.rbxm",
            ".DS_Store",
            "notes.txt",
            "luaurc",
        ] {
            assert!(!changes_folder(name), "{name}");
        }
    }

    /// A project file that stops naming itself, read again while serving,
    /// keeps the name it was served with instead of stopping the server, and
    /// its new rules apply.
    #[test]
    fn reloading_a_project_without_a_name_keeps_the_old_one() {
        let job = project_job(
            "game.project.json",
            r#"{"name":"Game","tree":{"$className":"Folder"}}"#,
            Vec::new(),
        );
        job.vfs
            .write(
                "/project/game.project.json",
                r#"{"tree":{"$className":"Folder"},"syncbackRules":{"ignoreProperties":{"Part":["Color"]}}}"#,
            )
            .unwrap();
        job.reload_project();
        let project = job.project();
        assert_eq!(project.name.as_deref(), Some("Game"));
        assert!(project.syncback_rules.is_some());
    }

    /// A late event for a file that's gone again leaves the `.gitkeep` of a
    /// folder that's still empty.
    #[test]
    fn late_events_leave_a_still_needed_gitkeep() {
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([(
                    "Holder",
                    VfsSnapshot::dir([(".gitkeep", VfsSnapshot::file(""))]),
                )]),
            )],
            "/project/src",
        );
        job.handle_vfs_event(VfsEvent::Create("/project/src/Holder/Gone.luau".into()));
        assert!(exists(&job, "/project/src/Holder/.gitkeep"));
    }

    /// A folder a project maps with `$path` keeps its `.gitkeep` too when a
    /// deletion from Studio leaves it empty.
    #[test]
    fn deleting_the_last_child_keeps_a_project_mapped_folder() {
        let job = project_job(
            "default.project.json",
            r#"{"name":"Test","tree":{"$className":"Folder","Shared":{"$path":"src/shared"}}}"#,
            vec![(
                "src",
                VfsSnapshot::dir([(
                    "shared",
                    VfsSnapshot::dir([("Only.luau", VfsSnapshot::file("return 1"))]),
                )]),
            )],
        );
        let only = find(&job, &["Shared", "Only"]);
        let (accepted, _, errors) = studio_write(
            &job,
            PatchSet {
                removed_instances: vec![only],
                ..PatchSet::new()
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(accepted.removed_instances, vec![only]);
        assert!(exists(&job, "/project/src/shared/.gitkeep"));
    }

    fn part_job() -> (JobThreadContext, Ref) {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project/default.project.json",
            VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
        )
        .unwrap();
        imfs.load_snapshot("/project/Part", VfsSnapshot::empty_dir())
            .unwrap();
        imfs.load_snapshot(
            "/project/Part/init.meta.json",
            VfsSnapshot::file(
                r#"{"className":"Part","properties":{"Transparency":0.25,"CanCollide":false}}"#,
            ),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let child = snapshot_from_vfs(&Default::default(), &vfs, &PathBuf::from("/project/Part"))
            .unwrap()
            .unwrap();
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![child]));
        let id = tree.root().children()[0];
        (
            JobThreadContext {
                tree: Arc::new(Mutex::new(tree)),
                vfs,
                message_queue: Arc::new(MessageQueue::new()),
                project: Arc::new(project).into(),
            },
            id,
        )
    }

    /// Builds a job whose tree holds the instance at `source` as the root's
    /// only child, with `files` loaded under `/project`.
    fn script_job(files: Vec<(&str, VfsSnapshot)>, source: &str) -> (JobThreadContext, Ref) {
        let mut entries = files;
        entries.push((
            "default.project.json",
            VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
        ));
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/project", VfsSnapshot::dir(entries))
            .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let child = snapshot_from_vfs(&Default::default(), &vfs, Path::new(source))
            .unwrap()
            .unwrap();
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![child]));
        let id = tree.root().children()[0];
        (
            JobThreadContext {
                tree: Arc::new(Mutex::new(tree)),
                vfs,
                message_queue: Arc::new(MessageQueue::new()),
                project: Arc::new(project).into(),
            },
            id,
        )
    }

    fn property_update(id: Ref, key: &str, value: Variant) -> PatchSet {
        PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(ustr(key), Some(value))]),
                changed_metadata: None,
            }],
        }
    }

    fn sound_alias_job(
        storage: &str,
        properties: &str,
    ) -> (JobThreadContext, Ref, &'static str, &'static str) {
        let model = format!(r#"{{"className":"Sound","properties":{{{properties}}}}}"#);
        let (job, id, source, disk) = match storage {
            "project" | "project-path" => {
                let path = if storage == "project-path" {
                    r#""$path":"Sound","#
                } else {
                    r#""$className":"Sound","#
                };
                let files = if storage == "project-path" {
                    vec![(
                        "Sound",
                        VfsSnapshot::dir([(
                            "init.meta.json",
                            VfsSnapshot::file(
                                r#"{"className":"Sound","properties":{"RollOffMinDistance":3}}"#,
                            ),
                        )]),
                    )]
                } else {
                    Vec::new()
                };
                let job = project_job(
                    "default.project.json",
                    &format!(
                        r#"{{"name":"Test","tree":{{"$className":"Folder","Sound":{{{path}"$properties":{{{properties}}}}}}}}}"#
                    ),
                    files,
                );
                let id = find(&job, &["Sound"]);
                (
                    job,
                    id,
                    "/project/default.project.json",
                    "/project/default.project.json",
                )
            }
            "directory" | "inline" => {
                let metadata = if storage == "inline" {
                    format!(
                        r#"{{"children":[{{"name":"Sound","className":"Sound","properties":{{{properties}}}}}]}}"#
                    )
                } else {
                    model.clone()
                };
                let (job, owner) = script_job(
                    vec![(
                        "Sound",
                        VfsSnapshot::dir([("init.meta.json", VfsSnapshot::file(metadata))]),
                    )],
                    "/project/Sound",
                );
                let id = if storage == "inline" {
                    job.tree
                        .lock()
                        .unwrap()
                        .get_instance(owner)
                        .unwrap()
                        .children()[0]
                } else {
                    owner
                };
                (job, id, "/project/Sound", "/project/Sound/init.meta.json")
            }
            "model" | "adjacent" => {
                let files = if storage == "adjacent" {
                    vec![
                        (
                            "Sound.rbxmx",
                            VfsSnapshot::file(
                                r#"<roblox version="4"><Item class="Sound" referent="0"><Properties><float name="RollOffMinDistance">3</float></Properties></Item></roblox>"#,
                            ),
                        ),
                        (
                            "Sound.meta.json",
                            VfsSnapshot::file(format!(r#"{{"properties":{{{properties}}}}}"#)),
                        ),
                    ]
                } else {
                    vec![("Sound.model.json", VfsSnapshot::file(model))]
                };
                let source = if storage == "adjacent" {
                    "/project/Sound.rbxmx"
                } else {
                    "/project/Sound.model.json"
                };
                let (job, id) = script_job(files, source);
                let disk = if storage == "adjacent" {
                    "/project/Sound.meta.json"
                } else {
                    "/project/Sound.model.json"
                };
                (job, id, source, disk)
            }
            _ => panic!("Unknown test storage {storage}"),
        };
        (job, id, source, disk)
    }

    #[test]
    fn sound_alias_ingestion_is_canonical_before_each_overlay() {
        for storage in [
            "model",
            "directory",
            "inline",
            "project",
            "adjacent",
            "project-path",
        ] {
            for (properties, expected) in [
                (r#""EmitterSize":5"#, 5.0),
                (r#""EmitterSize":5,"RollOffMinDistance":7"#, 7.0),
                (r#""RollOffMinDistance":7,"EmitterSize":5"#, 7.0),
            ] {
                let (job, id, _, _) = sound_alias_job(storage, properties);
                let tree = job.tree.lock().unwrap();
                let sound = tree.get_instance(id).unwrap();
                assert_eq!(
                    sound.properties().get(&ustr("EmitterSize")),
                    None,
                    "{storage}, {properties}"
                );
                assert_eq!(
                    sound.properties().get(&ustr("RollOffMinDistance")),
                    Some(&Variant::Float32(expected)),
                    "{storage}, {properties}"
                );
            }
        }
    }

    #[test]
    fn sound_alias_canonical_writes_reload_without_stale_keys() {
        // Project nodes intentionally refuse two-way writes. Their serialization is
        // covered through full syncback instead, without changing that authority policy.
        for storage in ["model", "directory", "inline", "adjacent"] {
            for value in [10.0, 7.0] {
                let (job, id, source, disk) = sound_alias_job(storage, r#""EmitterSize":5"#);
                let (accepted, conflicts, errors) = job.handle_tree_event(property_update(
                    id,
                    "RollOffMinDistance",
                    Variant::Float32(value),
                ));
                assert!(errors.is_empty(), "{storage}, {value}, {errors:?}");
                assert_eq!(conflicts, PatchSet::new(), "{storage}, {value}");
                assert_eq!(accepted.updated_instances.len(), 1, "{storage}, {value}");
                if storage == "adjacent" {
                    // Native models absorb sidecar properties and remove the empty
                    // sidecar, so it cannot reapply the old alias on the next load.
                    assert!(job.vfs.read(disk).with_not_found().unwrap().is_none());
                } else {
                    let text = job.vfs.read_to_string(disk).unwrap();
                    assert!(!text.contains("EmitterSize"), "{storage}, {value}, {text}");
                }
                let reloaded =
                    snapshot_from_vfs(&Default::default(), &job.vfs, Path::new(source)).unwrap();
                let tree = job.tree.lock().unwrap();
                let owner = tree.root().children()[0];
                assert_eq!(
                    compute_patch_set(reloaded, &tree, owner),
                    PatchSet::new(),
                    "{storage}, {value}"
                );
                let sound = tree.get_instance(id).unwrap();
                assert!(!sound.properties().contains_key(&ustr("EmitterSize")));
                assert_eq!(
                    sound
                        .properties()
                        .get(&ustr("RollOffMinDistance"))
                        .cloned()
                        .unwrap_or(Variant::Float32(10.0)),
                    Variant::Float32(value),
                    "{storage}, {value}"
                );
            }
        }
    }

    #[test]
    fn sound_alias_exclusions_refuse_writes_without_disk_mutation() {
        for storage in [
            "model",
            "directory",
            "inline",
            "project",
            "project-path",
            "adjacent",
        ] {
            for class in ["Sound", "Instance"] {
                for ignored in ["EmitterSize", "RollOffMinDistance"] {
                    let (mut job, id, source, disk) =
                        sound_alias_job(storage, r#""EmitterSize":5"#);
                    Arc::get_mut(job.project.project.get_mut().unwrap())
                        .unwrap()
                        .syncback_rules = Some(
                        serde_json::from_value(serde_json::json!({
                            "ignoreProperties": {class: [ignored]}
                        }))
                        .unwrap(),
                    );
                    let before = job.vfs.read(disk).unwrap();
                    let source_before = job.vfs.read(source).ok();
                    let (accepted, _, errors) = job.handle_tree_event(property_update(
                        id,
                        "RollOffMinDistance",
                        Variant::Float32(7.0),
                    ));
                    assert!(
                        accepted.updated_instances.is_empty(),
                        "{storage}, {class}, {ignored}"
                    );
                    assert!(!errors.is_empty(), "{storage}, {class}, {ignored}");
                    assert_eq!(
                        job.vfs.read(disk).unwrap(),
                        before,
                        "{storage}, {class}, {ignored}"
                    );
                    assert_eq!(
                        job.vfs.read(source).ok(),
                        source_before,
                        "{storage}, {class}, {ignored}"
                    );
                }
            }
        }
    }

    #[test]
    fn rounded_defaults_do_not_conflict_with_the_next_update_in_a_batch() {
        let (job, id) = part_job();
        let mut patch = property_update(id, "Transparency", Variant::Float32(0.0000005));
        patch
            .updated_instances
            .extend(property_update(id, "Transparency", Variant::Float32(0.5)).updated_instances);
        let (accepted, conflicts, errors) = job.handle_tree_event(patch);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 2);
        assert_eq!(
            accepted.updated_instances[0]
                .changed_properties
                .get(&ustr("Transparency")),
            Some(&None),
        );
        let snapshot =
            snapshot_from_vfs(&Default::default(), &job.vfs, Path::new("/project/Part")).unwrap();
        let tree = job.tree.lock().unwrap();
        assert_eq!(compute_patch_set(snapshot, &tree, id), PatchSet::new());
    }

    #[test]
    fn rewriting_untouched_numbers_reconciles_the_entire_owner_before_the_next_edit() {
        for inline in [false, true] {
            let mut metadata = serde_json::json!({
                "className": "Part",
                "properties": {"Transparency": 0.0000005},
            });
            if inline {
                metadata["children"] = serde_json::json!([{
                    "name": "Inline",
                    "className": "Part",
                    "properties": {"Transparency": 0.0000005},
                }]);
            }
            let (job, owner) = script_job(
                vec![(
                    "Part",
                    VfsSnapshot::dir([("init.meta.json", VfsSnapshot::file(metadata.to_string()))]),
                )],
                "/project/Part",
            );
            let target = if inline {
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(owner)
                    .unwrap()
                    .children()[0]
            } else {
                owner
            };
            let mut patch = property_update(owner, "Reflectance", Variant::Float32(0.25));
            patch.updated_instances.extend(
                property_update(target, "Transparency", Variant::Float32(0.5)).updated_instances,
            );
            let (accepted, conflicts, errors) = job.handle_tree_event(patch);
            assert!(errors.is_empty(), "inline {inline}, {errors:?}");
            assert_eq!(conflicts, PatchSet::new(), "inline {inline}");
            assert_eq!(accepted.updated_instances.len(), 2, "inline {inline}");
            let snapshot =
                snapshot_from_vfs(&Default::default(), &job.vfs, Path::new("/project/Part"))
                    .unwrap();
            let tree = job.tree.lock().unwrap();
            assert_eq!(compute_patch_set(snapshot, &tree, owner), PatchSet::new());
        }
    }

    #[test]
    fn rewriting_metadata_normalizes_preserved_float32_boundary_values() {
        for boundary in [2.000001, 2.0000002] {
            for inline in [false, true] {
                let mut metadata = serde_json::json!({
                    "className": "Part",
                    "properties": {"Size": [boundary, 3, 4]},
                    "attributes": {"Boundary": {"Vector3": [boundary, 3, 4]}},
                });
                if inline {
                    metadata = serde_json::json!({"className": "Folder", "children": [metadata]});
                }
                let (job, owner) = script_job(
                    vec![(
                        "Part",
                        VfsSnapshot::dir([(
                            "init.meta.json",
                            VfsSnapshot::file(metadata.to_string()),
                        )]),
                    )],
                    "/project/Part",
                );
                let id = if inline {
                    job.tree
                        .lock()
                        .unwrap()
                        .get_instance(owner)
                        .unwrap()
                        .children()[0]
                } else {
                    owner
                };
                let (accepted, conflicts, errors) = job.handle_tree_event(property_update(
                    id,
                    "Reflectance",
                    Variant::Float32(0.25),
                ));
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(conflicts, PatchSet::new());
                assert_eq!(accepted.updated_instances.len(), 1);
                let contents = read_text(&job.vfs, "/project/Part/init.meta.json").unwrap();
                let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
                let metadata = if inline {
                    &metadata["children"][0]
                } else {
                    &metadata
                };
                assert_eq!(metadata["properties"]["Size"], serde_json::json!([2, 3, 4]));
                assert_eq!(
                    metadata["attributes"]["Boundary"]["Vector3"],
                    serde_json::json!([2, 3, 4])
                );
                let expected = Variant::Vector3(Vector3::new(2.0, 3.0, 4.0));
                let tree = job.tree.lock().unwrap();
                assert_eq!(
                    tree.get_instance(id).unwrap().properties()[&ustr("Size")],
                    expected
                );
                let (_, messages) = job.message_queue.subscribe(0).try_recv().unwrap().unwrap();
                assert!(messages
                    .iter()
                    .flat_map(|message| &message.updated)
                    .any(|update| {
                        update.id == id
                            && update.changed_properties.get(&ustr("Size"))
                                == Some(&Some(expected.clone()))
                    }));
            }
        }
    }

    #[test]
    fn already_saved_numbers_publish_only_needed_normalization_corrections() {
        for (metadata, input, correction) in [
            (
                r#"{"className":"NumberValue","properties":{"Value":5}}"#,
                5.0000005,
                Some(Some(Variant::Float64(5.0))),
            ),
            (r#"{"className":"NumberValue"}"#, 0.0000005, Some(None)),
            (
                r#"{"className":"NumberValue","properties":{"Value":5}}"#,
                5.0,
                None,
            ),
            (r#"{"className":"NumberValue"}"#, 0.0, None),
            (
                r#"{"className":"NumberValue","properties":{"Value":{"Float64":"nan"}}}"#,
                f64::NAN,
                None,
            ),
        ] {
            let (job, id) = script_job(
                vec![(
                    "Number",
                    VfsSnapshot::dir([("init.meta.json", VfsSnapshot::file(metadata))]),
                )],
                "/project/Number",
            );
            let (accepted, conflicts, errors) =
                job.handle_tree_event(property_update(id, "Value", Variant::Float64(input)));
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(accepted, PatchSet::new());
            assert_eq!(conflicts, PatchSet::new());
            assert_eq!(
                read_text(&job.vfs, "/project/Number/init.meta.json").unwrap(),
                metadata
            );
            let messages = job.message_queue.subscribe(0).try_recv().unwrap();
            assert_eq!(messages.is_some(), correction.is_some(), "{input}");
            if let Some(expected) = correction {
                let (_, messages) = messages.unwrap();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].updated.len(), 1);
                assert_eq!(
                    messages[0].updated[0]
                        .changed_properties
                        .get(&ustr("Value")),
                    Some(&expected)
                );
            }
        }
    }

    fn read_text(vfs: &Vfs, path: &str) -> Option<String> {
        vfs.read(path)
            .with_not_found()
            .unwrap()
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn relocation_job() -> (JobThreadContext, Ref, Ref, Ref) {
        let (job, root) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([
                    (
                        "Left",
                        VfsSnapshot::dir([
                            ("Code.server.lua", VfsSnapshot::file("print('original')")),
                            (
                                "Code.meta.jsonc",
                                VfsSnapshot::file(r#"{"properties":{"Disabled":true}}"#),
                            ),
                            (
                                "Folder",
                                VfsSnapshot::dir([
                                    ("Child.luau", VfsSnapshot::file("return 1")),
                                    ("unmanaged.bin", VfsSnapshot::file("private bytes")),
                                ]),
                            ),
                        ]),
                    ),
                    ("Right", VfsSnapshot::dir([] as [(&str, VfsSnapshot); 0])),
                ]),
            )],
            "/project/src",
        );
        let left = find(&job, &["Left"]);
        let right = find(&job, &["Right"]);
        (job, root, left, right)
    }

    fn package_relocation_job(root: &Path, installed: bool) -> JobThreadContext {
        let manifest = "[package]\nname = \"scope/app\"\nversion = \"1.0.0\"\nrealm = \"shared\"\n[dependencies]\nFirst = \"^Local/First\"\nSecond = \"^Local/Second\"\n";
        let local_manifest = "[package]\nname = \"scope/library\"\nrealm = \"shared\"\n";
        let mut local_manifests = serde_json::Map::new();
        let mut package_roots = Vec::new();
        for name in ["First", "Second"] {
            let source = format!("Local/{name}");
            std::fs::create_dir_all(root.join(&source)).unwrap();
            std::fs::create_dir_all(root.join(format!("Packages/_Index/{name}/library"))).unwrap();
            std::fs::write(root.join(&source).join("wally.toml"), local_manifest).unwrap();
            local_manifests.insert(
                format!("{source}/wally.toml"),
                crate::package_sources::sha256(local_manifest.as_bytes()).into(),
            );
            package_roots.push(serde_json::json!({
                "name": "scope/library", "kind": "local", "realm": "shared",
                "install_path": format!("Packages/_Index/{name}/library"),
                "source_path": source,
            }));
        }
        for owner in ["App", "Local/First", "Local/Second"] {
            for folder in ["Left", "Right"] {
                std::fs::create_dir_all(root.join(owner).join(folder)).unwrap();
            }
            std::fs::write(root.join(owner).join("Left/Code.luau"), "return 1\n").unwrap();
            std::fs::write(root.join(owner).join("Right/Target.luau"), "return 2\n").unwrap();
        }
        std::fs::write(root.join("wally.toml"), manifest).unwrap();
        std::fs::write(
            root.join("wally.install.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "manifest_hash": crate::package_sources::sha256(manifest.as_bytes()),
                "local_manifests": local_manifests,
                "package_roots": package_roots,
                "generated_paths": [],
            }))
            .unwrap(),
        )
        .unwrap();
        let project = r#"{"name":"Test","tree":{"$className":"Folder","App":{"$path":"App"},"First":{"$path":"Packages/_Index/First/library"},"Second":{"$path":"Packages/_Index/Second/library"}}}"#;
        let project = if installed {
            project.to_owned()
        } else {
            project
                .replace("Packages/_Index/First/library", "Local/First")
                .replace("Packages/_Index/Second/library", "Local/Second")
        };
        std::fs::write(root.join("default.project.json"), project).unwrap();
        let job = disk_job(root);
        let mut context = crate::snapshot::InstanceContext::default();
        context.package_sources =
            crate::package_sources::PackageSources::load(&job.vfs, job.project().folder_location())
                .unwrap()
                .map(Arc::new);
        assert!(context.package_sources.as_ref().unwrap().problem.is_none());
        let path = job.project().folder_location().join("default.project.json");
        let snapshot = snapshot_from_vfs(&context, &job.vfs, &path)
            .unwrap()
            .unwrap();
        *job.tree.lock().unwrap() = RojoTree::new(
            InstanceSnapshot::new()
                .name("Root")
                .metadata(crate::snapshot::InstanceMetadata::new().context(&context))
                .children(vec![snapshot]),
        );
        job
    }

    #[test]
    fn repeated_installed_mount_events_preserve_local_identity_and_cursor() {
        for redirected in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let job = package_relocation_job(temp.path(), true);
            let root = job.project().folder_location().to_path_buf();
            if redirected {
                job.vfs
                    .write(
                        root.join("Local/First/default.project.json"),
                        r#"{"tree":{"$path":"Left"}}"#,
                    )
                    .unwrap();
            }
            let project_path = root.join("default.project.json");
            job.vfs
                .write(
                    &project_path,
                    r#"{"name":"Test","tree":{"$path":"Packages"}}"#,
                )
                .unwrap();
            let context = job.tree.lock().unwrap().root().metadata().context.clone();
            let snapshot = snapshot_from_vfs(&context, &job.vfs, &project_path)
                .unwrap()
                .unwrap();
            *job.tree.lock().unwrap() = RojoTree::new(
                InstanceSnapshot::new()
                    .name("Root")
                    .metadata(InstanceMetadata::new().context(&context))
                    .children(vec![snapshot]),
            );
            let id = find(&job, &["_Index", "First", "library"]);
            let count = job.tree.lock().unwrap().inner().descendants().count();
            let before = job.message_queue.cursor();
            let installed = job
                .vfs
                .canonicalize(root.join("Packages/_Index/First/library"))
                .unwrap();

            for event in [
                VfsEvent::Create(installed.clone()),
                VfsEvent::Write(installed.clone()),
                VfsEvent::Create(installed),
            ] {
                job.handle_vfs_event(event);
                assert_eq!(find(&job, &["_Index", "First", "library"]), id);
                assert_eq!(
                    job.tree.lock().unwrap().inner().descendants().count(),
                    count
                );
                assert_eq!(job.message_queue.cursor(), before);
            }

            let source = job
                .vfs
                .canonicalize(root.join("Local/First/Left/Code.luau"))
                .unwrap();
            let code = job.tree.lock().unwrap().get_ids_at_path(&source)[0];
            job.vfs.write(&source, "return 91\n").unwrap();
            job.handle_vfs_event(VfsEvent::Write(source.clone()));
            assert!(job.message_queue.cursor() > before);
            assert_eq!(
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(code)
                    .unwrap()
                    .properties()[&ustr("Source")],
                Variant::String("return 91\n".into())
            );
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                property_update(code, "Source", Variant::String("return 92\n".into())),
                Some(before),
                WriteAuthority::Filesystem,
            );
            assert_eq!(accepted, PatchSet::new());
            assert_eq!(conflicts, PatchSet::new());
            assert!(errors
                .iter()
                .any(|error| error.message.contains("baseline is stale")));
            assert_eq!(
                job.vfs.read_to_string(&source).unwrap().as_str(),
                "return 91\n"
            );
        }
    }

    #[test]
    fn watcher_mutations_and_publication_share_the_tree_revision_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let job = package_relocation_job(temp.path(), true);
        let path = job
            .vfs
            .canonicalize(temp.path().join("Local/First/Left/Code.luau"))
            .unwrap();
        let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
        let before = job.message_queue.cursor();
        job.vfs.write(&path, "return 73\n").unwrap();

        let changes = job.apply_patches_with_publication(path.clone(), |tree, changes| {
            assert!(!changes.is_empty());
            assert_eq!(job.message_queue.cursor(), before);
            assert!(matches!(
                job.tree.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            assert_eq!(
                tree.get_instance(id).unwrap().properties()[&ustr("Source")],
                Variant::String("return 73\n".into())
            );
        });

        assert_eq!(job.message_queue.cursor(), before + changes.len() as u32);
        let published = job.message_queue.cursor();
        assert!(job.apply_patches(path.clone()).is_empty());
        assert!(job.apply_patches(path).is_empty());
        assert_eq!(job.message_queue.cursor(), published);
    }

    fn event_changes_require_context(job: &JobThreadContext, path: &Path) -> bool {
        let mut tree = job.tree.lock().unwrap();
        let mut changes_mapping = false;
        job.for_each_fs_change(&mut tree, path, |tree, patch| {
            changes_mapping |= changes_require_context(tree, &patch);
        });
        changes_mapping
    }

    #[test]
    fn multi_mount_rereads_cannot_skip_mapping_validation() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root"));
        let id = tree.get_root_id();
        for source_count in [1, 2, 3] {
            assert!(!requires_mapping_transaction(&tree, &[], source_count));
            let patches = [property_update(
                id,
                "Source",
                Variant::String("return 2".into()),
            )];
            assert_eq!(
                requires_mapping_transaction(&tree, &patches, source_count),
                source_count > 1,
                "A property-only preflight cannot predict a later multi-mount filesystem read"
            );
            let patches = [rename_update(id, "Renamed")];
            assert!(requires_mapping_transaction(&tree, &patches, source_count));
        }
    }

    #[test]
    fn mapping_refresh_classifies_patch_inputs_not_file_extensions() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root"));
        let id = tree.get_root_id();
        for flags in 0..256 {
            let mut patch = PatchSet::new();
            let mut update = empty_update(id);
            if flags & 1 != 0 {
                update.changed_name = Some("Renamed".into());
            }
            if flags & 2 != 0 {
                update.changed_class_name = Some(ustr("DataModel"));
            }
            if flags & 4 != 0 {
                update.changed_parent = Some(Ref::new());
            }
            if flags & 8 != 0 {
                patch.added_instances.push(PatchAdd {
                    parent_id: id,
                    instance: InstanceSnapshot::new().name("Added"),
                });
            }
            if flags & 16 != 0 {
                patch.removed_instances.push(id);
            }
            let mut metadata = tree.get_metadata(id).unwrap().clone();
            if flags & 32 != 0 {
                metadata.installed_paths.push("/root/Installed".into());
            }
            if flags & 64 != 0 {
                metadata.context.emit_legacy_scripts = !metadata.context.emit_legacy_scripts;
            }
            if flags & 128 != 0 {
                metadata.relevant_paths.push("/root/Source".into());
            }
            // These are real updates, but do not alter import targets.
            metadata.ignore_unknown_instances = !metadata.ignore_unknown_instances;
            metadata.source_transform = Some(crate::snapshot::SourceTransform {
                original: "return 1".into(),
                emitted: "return 1".into(),
            });
            update.changed_metadata = Some(metadata);
            update
                .changed_properties
                .insert(ustr("Transparency"), Some(Variant::Float32(0.5)));
            patch.updated_instances.push(update);
            assert_eq!(
                changes_require_context(&tree, &patch),
                flags != 0,
                "flags {flags}"
            );
        }
    }

    fn mapping_refresh_job(root: &Path) -> JobThreadContext {
        std::fs::create_dir_all(root.join("Source/Part")).unwrap();
        std::fs::write(root.join("Source/App.luau"), "return 1\n").unwrap();
        std::fs::write(root.join("Source/Unrelated.luau"), "return 42\n").unwrap();
        std::fs::write(
            root.join("Source/Part/init.meta.json"),
            r#"{"className":"Part","properties":{"Transparency":0.1}}"#,
        )
        .unwrap();
        std::fs::write(root.join("default.project.json"), r#"{"name":"Test","globIgnorePaths":["Source/ignored","Source/ignored/**"],"tree":{"$className":"Folder","Source":{"$path":"Source"}}}"#).unwrap();
        let job = disk_job(root);
        let mut context = crate::snapshot::InstanceContext::default();
        context.set_reserved_cache_path(job.project().folder_location().join(".rojo"));
        let mut snapshot = snapshot_from_vfs(&context, &job.vfs, &job.project().file_location)
            .unwrap()
            .unwrap();
        let require_context =
            crate::serve_session::build_require_context(&snapshot, job.project().folder_location())
                .unwrap();
        crate::snapshot_middleware::prepare_requires(&mut snapshot, &job.vfs, &require_context)
            .unwrap();
        *job.tree.lock().unwrap() = RojoTree::new(snapshot);
        job
    }

    #[test]
    fn non_mapping_events_preserve_unrelated_source_allocations() {
        let temp = tempfile::tempdir().unwrap();
        let job = mapping_refresh_job(temp.path());
        let (unrelated, source_address) = {
            let tree = job.tree.lock().unwrap();
            let instance = tree
                .descendants(tree.get_root_id())
                .find(|instance| instance.name() == "Unrelated")
                .unwrap();
            let Variant::String(source) = &instance.properties()[&ustr("Source")] else {
                panic!("Expected source text")
            };
            (instance.id(), source.as_ptr() as usize)
        };
        let root = job.project().folder_location().to_path_buf();
        for (path, contents, changes_revision) in [
            ("Source/App.luau", "return 2\n", true),
            (
                "Source/Part/init.meta.json",
                r#"{"className":"Part","properties":{"Transparency":0.5}}"#,
                true,
            ),
            (
                "Source/Part/init.meta.json",
                r#"{"className":"Part","properties":{"Transparency":0.5}}"#,
                false,
            ),
            ("Source/ignored/Generated.luau", "return 3\n", false),
            ("Source/notes.tmp", "unrecognized", false),
            ("sourcemap.json", "{}", false),
        ] {
            let path = root.join(path);
            job.vfs.create_dir_all(path.parent().unwrap()).unwrap();
            job.vfs.write(&path, contents).unwrap();
            assert!(
                !event_changes_require_context(&job, &path),
                "{}",
                path.display()
            );
            let before = job.message_queue.cursor();
            job.handle_vfs_event(VfsEvent::Write(path));
            assert_eq!(job.message_queue.cursor() > before, changes_revision);
            let tree = job.tree.lock().unwrap();
            let Variant::String(source) =
                &tree.get_instance(unrelated).unwrap().properties()[&ustr("Source")]
            else {
                panic!("Expected source text")
            };
            assert_eq!(
                source.as_ptr() as usize,
                source_address,
                "Unrelated source must not be cloned"
            );
        }
        let tree = job.tree.lock().unwrap();
        let app = tree
            .descendants(tree.get_root_id())
            .find(|instance| instance.name() == "App")
            .unwrap();
        assert_eq!(
            app.properties()[&ustr("Source")],
            Variant::String("return 2\n".into())
        );
        let part = tree
            .descendants(tree.get_root_id())
            .find(|instance| instance.name() == "Part")
            .unwrap();
        assert_eq!(
            part.properties()[&ustr("Transparency")],
            Variant::Float32(0.5)
        );
    }

    #[test]
    fn ordinary_property_policy_classifies_every_patch_flag_combination() {
        for mask in 0..128 {
            let id = Ref::new();
            let mut update = empty_update(id);
            if mask & 1 != 0 {
                update.changed_name = Some("Renamed".into());
            }
            if mask & 2 != 0 {
                update.changed_parent = Some(Ref::new());
            }
            if mask & 4 != 0 {
                update.changed_class_name = Some(ustr("Part"));
            }
            if mask & 8 != 0 {
                update.changed_metadata = Some(crate::snapshot::InstanceMetadata::new());
            }
            if mask & 16 == 0 {
                update.changed_properties.insert(
                    ustr("Transparency"),
                    Some(if mask & 32 != 0 {
                        Variant::Ref(Ref::new())
                    } else {
                        Variant::Float32(0.5)
                    }),
                );
            }
            if mask & 64 != 0 {
                update
                    .changed_properties
                    .insert(ustr("Source"), Some(Variant::String("return 1".into())));
            }
            assert_eq!(
                is_ordinary_property_update(&update, "ModuleScript", None),
                mask == 0,
                "{mask}"
            );
        }
        for (class, middleware, property, ordinary) in [
            ("StringValue", Some(Middleware::Text), "Value", false),
            (
                "LocalizationTable",
                Some(Middleware::Csv),
                "Contents",
                false,
            ),
            (
                "LocalizationTable",
                Some(Middleware::CsvDir),
                "Contents",
                false,
            ),
            (
                "ModuleScript",
                Some(Middleware::ModuleScript),
                "Source",
                false,
            ),
            ("NumberValue", Some(Middleware::Dir), "Value", true),
            ("StringValue", Some(Middleware::Dir), "Value", true),
            ("Part", Some(Middleware::Dir), "Size", true),
        ] {
            let update = property_update(Ref::new(), property, Variant::String("value".into()));
            assert_eq!(
                is_ordinary_property_update(&update.updated_instances[0], class, middleware),
                ordinary
            );
        }
    }

    #[test]
    fn scaling_still_rejects_unseen_structural_changes_to_its_target() {
        for delivered in [false, true] {
            for class in ["Part", "MeshPart"] {
                let temp = tempfile::tempdir().unwrap();
                let job = mapping_refresh_job(temp.path());
                let path = job
                    .project()
                    .folder_location()
                    .join("Source/Part/init.meta.json");
                let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
                let cursor = job.message_queue.cursor();
                let metadata = serde_json::json!({"className":class,"name":"DiskPart","properties":{"Transparency":0.7}});
                job.vfs
                    .write(&path, serde_json::to_vec(&metadata).unwrap())
                    .unwrap();
                if delivered {
                    job.handle_vfs_event(VfsEvent::Write(path.clone()));
                }
                let before = job.vfs.read(&path).unwrap();
                let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                    property_update(id, "Transparency", Variant::Float32(0.2)),
                    Some(cursor),
                    WriteAuthority::Filesystem,
                );
                assert!(errors.is_empty(), "{errors:?}");
                assert!(accepted.updated_instances.is_empty());
                assert!(!conflicts.updated_instances.is_empty());
                assert_eq!(job.vfs.read(&path).unwrap(), before);
            }
        }
    }

    #[test]
    fn ordinary_properties_survive_alias_save_bookkeeping() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".luaurc"),
            r#"{"aliases":{"target":"Source/Unrelated"}}"#,
        )
        .unwrap();
        let job = mapping_refresh_job(temp.path());
        let path = job.project().folder_location().join("Source/App.luau");
        let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
        let cursor = job.message_queue.cursor();
        let authored = "return require(\"@target\")\n";
        job.vfs.write(&path, authored).unwrap();
        job.handle_vfs_event(VfsEvent::Write(path.clone()));
        assert!(job.message_queue.cursor() > cursor);
        {
            let tree = job.tree.lock().unwrap();
            let transform = tree
                .get_metadata(id)
                .unwrap()
                .source_transform
                .as_ref()
                .unwrap();
            assert_eq!(transform.original, authored);
            assert_ne!(transform.emitted, authored);
        }
        let mut attributes = Attributes::new();
        attributes.insert("Revision".into(), Variant::Float64(2.0));
        let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
            property_update(id, "Attributes", Variant::Attributes(attributes.clone())),
            Some(cursor),
            WriteAuthority::Filesystem,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
        assert_eq!(accepted.updated_instances.len(), 1);
        assert_eq!(
            job.tree
                .lock()
                .unwrap()
                .get_instance(id)
                .unwrap()
                .properties()[&ustr("Attributes")],
            Variant::Attributes(attributes)
        );
        assert_eq!(job.vfs.read(&path).unwrap().as_slice(), authored.as_bytes());
    }

    #[test]
    fn rapid_scaling_keeps_latest_studio_properties_with_an_older_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let job = mapping_refresh_job(temp.path());
        let path = job
            .project()
            .folder_location()
            .join("Source/Part/init.meta.json");
        let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
        let cursor = job.message_queue.cursor();
        for width in [2.0, 3.0, 4.0] {
            let size = Variant::Vector3(rbx_dom_weak::types::Vector3::new(width, 2.0, 2.0));
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                property_update(id, "Size", size.clone()),
                Some(cursor),
                WriteAuthority::Filesystem,
            );
            assert!(errors.is_empty(), "{errors:?}");
            assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
            assert_eq!(accepted.updated_instances.len(), 1);
            assert_eq!(
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(id)
                    .unwrap()
                    .properties()[&ustr("Size")],
                size
            );
            let saved: serde_json::Value =
                crate::json::from_slice(&job.vfs.read(&path).unwrap()).unwrap();
            assert_eq!(
                saved["properties"]["Size"][0].as_f64(),
                Some(f64::from(width))
            );
        }
    }

    #[test]
    fn scaling_merges_current_disk_properties_without_false_conflicts() {
        for delivered in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let job = mapping_refresh_job(temp.path());
            let path = job
                .project()
                .folder_location()
                .join("Source/Part/init.meta.json");
            let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
            let cursor = job.message_queue.cursor();
            job.vfs
                .write(
                    &path,
                    r#"{"className":"Part","properties":{"Transparency":0.7,"Size":[9,9,9]}}"#,
                )
                .unwrap();
            if delivered {
                job.handle_vfs_event(VfsEvent::Write(path.clone()));
            }
            let size = Variant::Vector3(rbx_dom_weak::types::Vector3::new(3.0, 2.0, 2.0));
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                property_update(id, "Size", size.clone()),
                Some(cursor),
                WriteAuthority::Filesystem,
            );
            assert!(errors.is_empty(), "{errors:?}");
            assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
            assert_eq!(accepted.updated_instances.len(), 1);
            let tree = job.tree.lock().unwrap();
            let part = tree.get_instance(id).unwrap();
            assert_eq!(part.properties()[&ustr("Size")], size);
            assert_eq!(
                part.properties()[&ustr("Transparency")],
                Variant::Float32(0.7)
            );
        }
    }

    #[test]
    fn repeated_gizmo_properties_work_for_directory_inline_and_model_storage() {
        use rbx_dom_weak::types::{CFrame, Matrix3, Vector3};
        let (inline, _) = inline_job();
        let inline_id = find(&inline, &["Base"]);
        let (model, _, model_id) = model_job();
        for (job, id) in [part_job(), (inline, inline_id), (model, model_id)] {
            let cursor = job.message_queue.cursor();
            let instance_count = job.tree.lock().unwrap().inner().descendants().count();
            for position in [1.0, 2.0, 3.0] {
                let size = Variant::Vector3(Vector3::new(position + 2.0, 2.0, 2.0));
                let frame = Variant::CFrame(CFrame::new(
                    Vector3::new(position, 0.0, 0.0),
                    Matrix3::identity(),
                ));
                let mut patch = property_update(id, "Size", size.clone());
                patch.updated_instances[0]
                    .changed_properties
                    .insert(ustr("CFrame"), Some(frame.clone()));
                let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                    patch,
                    Some(cursor),
                    WriteAuthority::Filesystem,
                );
                assert!(errors.is_empty(), "{errors:?}");
                assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
                assert_eq!(accepted.updated_instances.len(), 1);
                let tree = job.tree.lock().unwrap();
                assert_eq!(tree.inner().descendants().count(), instance_count);
                let instance = tree.get_instance(id).unwrap();
                assert_eq!(instance.properties()[&ustr("Size")], size);
                assert_eq!(instance.properties()[&ustr("CFrame")], frame);
            }
        }
    }

    #[test]
    fn mapping_enabled_bidirectional_edits_keep_filesystem_authority() {
        let temp = tempfile::tempdir().unwrap();
        let job = mapping_refresh_job(temp.path());
        let path = job.project().folder_location().join("Source/App.luau");
        let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
        let stale_cursor = job.message_queue.cursor();
        job.vfs.write(&path, "return 2\n").unwrap();
        job.handle_vfs_event(VfsEvent::Write(path.clone()));
        assert!(job.message_queue.cursor() > stale_cursor);
        let update = |source: &str| property_update(id, "Source", Variant::String(source.into()));
        let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
            update("return 3\n"),
            Some(stale_cursor),
            WriteAuthority::Filesystem,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            conflicts.updated_instances[0].changed_properties[&ustr("Source")],
            Some(Variant::String("return 2\n".into()))
        );
        assert_eq!(
            job.vfs.read_to_string(&path).unwrap().as_str(),
            "return 2\n"
        );

        let (accepted, conflicts, errors) = job.handle_tree_event(update("return 4\n"));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(conflicts.updated_instances.is_empty());
        assert_eq!(accepted.updated_instances.len(), 1);
        assert_eq!(
            job.vfs.read_to_string(&path).unwrap().as_str(),
            "return 4\n"
        );
        let cursor = job.message_queue.cursor();
        job.handle_vfs_event(VfsEvent::Write(path.clone()));
        assert_eq!(
            job.message_queue.cursor(),
            cursor,
            "Writeback echoes must not publish another revision"
        );

        // A disk edit that the watcher has not consumed must win as well.
        job.vfs.write(&path, "return 5\n").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(update("return 6\n"));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            job.vfs.read_to_string(&path).unwrap().as_str(),
            "return 5\n"
        );
        assert_eq!(
            job.tree
                .lock()
                .unwrap()
                .get_instance(id)
                .unwrap()
                .properties()[&ustr("Source")],
            Variant::String("return 5\n".into())
        );
    }

    #[test]
    fn text_payload_edits_preserve_metadata_echoes_and_filesystem_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let job = mapping_refresh_job(temp.path());
        let path = job.project().folder_location().join("Source/Text.txt");
        let metadata_path = path.with_extension("meta.json");
        let metadata = "{\n  \"attributes\": { \"Authored\": true }\n}\n";
        job.vfs.write(&metadata_path, metadata).unwrap();
        job.vfs.write(&path, "created").unwrap();
        job.apply_patches(path.clone());
        let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
        for studio_value in ["edited in Studio", "", "edited again"] {
            job.vfs.write(&path, "filesystem edit").unwrap();
            job.handle_vfs_event(VfsEvent::Write(path.clone()));
            let (accepted, conflicts, errors) = job.handle_tree_event(property_update(
                id,
                "Value",
                Variant::String(studio_value.into()),
            ));
            assert!(errors.is_empty(), "{errors:?}");
            assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
            assert_eq!(accepted.updated_instances.len(), 1);
            assert_eq!(
                job.vfs.read_to_string(&path).unwrap().as_str(),
                studio_value
            );
            assert_eq!(
                job.vfs.read_to_string(&metadata_path).unwrap().as_str(),
                metadata
            );
            let cursor = job.message_queue.cursor();
            job.handle_vfs_event(VfsEvent::Write(path.clone()));
            assert_eq!(job.message_queue.cursor(), cursor);
            assert_eq!(
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(id)
                    .unwrap()
                    .properties()[&ustr("Value")],
                Variant::String(studio_value.into())
            );
        }
        let old_cursor = job.message_queue.cursor();
        job.vfs.write(&path, "unseen disk edit").unwrap();
        for delivered in [false, true] {
            if delivered {
                job.handle_vfs_event(VfsEvent::Write(path.clone()));
            }
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                property_update(
                    id,
                    "Value",
                    Variant::String("conflicting Studio edit".into()),
                ),
                Some(old_cursor),
                WriteAuthority::Filesystem,
            );
            assert!(errors.is_empty(), "{errors:?}");
            assert!(accepted.updated_instances.is_empty());
            assert_eq!(conflicts.updated_instances.len(), 1);
            assert_eq!(
                job.vfs.read_to_string(&path).unwrap().as_str(),
                "unseen disk edit"
            );
        }
    }

    #[test]
    fn localization_payload_edits_keep_the_csv_and_adjacent_metadata() {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let job = mapping_refresh_job(temp.path());
            let root = job.project().folder_location().join("Source");
            let (path, metadata) = if directory {
                job.vfs.create_dir(root.join("Translations")).unwrap();
                (
                    root.join("Translations/init.csv"),
                    root.join("Translations/init.meta.json"),
                )
            } else {
                (
                    root.join("Translations.csv"),
                    root.join("Translations.meta.json"),
                )
            };
            let authored_metadata = "{\n  \"attributes\": { \"Authored\": true }\n}\n";
            job.vfs.write(&metadata, authored_metadata).unwrap();
            job.vfs
                .write(&path, "Source,en\ninitial,initial\n")
                .unwrap();
            job.apply_patches(path.clone());
            let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
            let contents = r#"[{"source":"updated","values":{"en":"updated"}}]"#;
            let (accepted, conflicts, errors) = job.handle_tree_event(property_update(
                id,
                "Contents",
                Variant::String(contents.into()),
            ));
            assert!(errors.is_empty(), "{errors:?}");
            assert!(conflicts.updated_instances.is_empty(), "{conflicts:?}");
            assert_eq!(accepted.updated_instances.len(), 1);
            assert!(job.vfs.read_to_string(&path).unwrap().contains("updated"));
            assert_eq!(
                job.vfs.read_to_string(&metadata).unwrap().as_str(),
                authored_metadata
            );
        }
    }

    #[test]
    fn payload_edits_with_metadata_overrides_do_not_report_false_success() {
        for (filename, property, initial, overridden, edited) in [
            ("Text.txt", "Value", "disk", "override", "edited"),
            (
                "Module.luau",
                "Source",
                "return 1\n",
                "return 2\n",
                "return 3\n",
            ),
            (
                "Translations.csv",
                "Contents",
                "Source,en\ninitial,initial\n",
                r#"[{"source":"override","values":{"en":"override"}}]"#,
                r#"[{"source":"edited","values":{"en":"edited"}}]"#,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let job = mapping_refresh_job(temp.path());
            let path = job
                .project()
                .folder_location()
                .join("Source")
                .join(filename);
            let metadata_path = path.with_extension("meta.json");
            let metadata = serde_json::json!({ "properties": { property: overridden } });
            job.vfs
                .write(&metadata_path, serde_json::to_vec(&metadata).unwrap())
                .unwrap();
            job.vfs.write(&path, initial).unwrap();
            job.apply_patches(path.clone());
            let id = job.tree.lock().unwrap().get_ids_at_path(&path)[0];
            let (accepted, conflicts, errors) = job.handle_tree_event(property_update(
                id,
                property,
                Variant::String(edited.into()),
            ));
            // Removing an override and updating its payload requires two-file persistence.
            // Until that transaction is supported, reject without modifying either file.
            assert_eq!(errors.len(), 1, "{filename} {errors:?}");
            assert!(
                errors[0].message.contains("transactional persistence"),
                "{filename} {errors:?}"
            );
            assert_eq!(conflicts.updated_instances.len(), 1, "{filename}");
            assert!(accepted.updated_instances.is_empty(), "{filename}");
            assert_eq!(job.vfs.read_to_string(&path).unwrap().as_str(), initial);
            assert_eq!(
                job.vfs.read_to_string(&metadata_path).unwrap().as_str(),
                serde_json::to_string(&metadata).unwrap()
            );
            let cursor = job.message_queue.cursor();
            job.handle_vfs_event(VfsEvent::Write(path.clone()));
            job.handle_vfs_event(VfsEvent::Write(metadata_path));
            assert_eq!(job.message_queue.cursor(), cursor, "{filename}");
            let tree = job.tree.lock().unwrap();
            let value = &tree.get_instance(id).unwrap().properties()[&ustr(property)];
            if property == "Contents" {
                let Variant::String(value) = value else {
                    panic!("Expected localization JSON")
                };
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(value).unwrap(),
                    serde_json::from_str::<serde_json::Value>(overridden).unwrap()
                );
            } else {
                assert_eq!(value, &Variant::String(overridden.into()), "{filename}");
            }
        }
    }

    #[test]
    fn project_remounts_publish_fresh_imports_and_propagate_watcher_context() {
        for nested in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::create_dir_all(root.join("Source/Shared")).unwrap();
            std::fs::write(root.join("Source/Shared/Util.luau"), "return 1\n").unwrap();
            std::fs::write(root.join("App.luau"), "return require(\"@shared/Util\")\n").unwrap();
            std::fs::write(
                root.join(".luaurc"),
                r#"{"aliases":{"shared":"Source/Shared","future":"Source/Shared/Extra.luau"}}"#,
            )
            .unwrap();
            let service = serde_json::json!({
                "$className": "ReplicatedStorage",
                "Shared": { "$path": "Source/Shared" },
                "App": { "$path": "App.luau" }
            });
            let project = serde_json::json!({
                "name": "Test",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": if nested {
                        serde_json::json!({ "$path": "inner.project.json" })
                    } else { service.clone() }
                }
            });
            let root_project = root.join("default.project.json");
            std::fs::write(&root_project, serde_json::to_vec(&project).unwrap()).unwrap();
            let changed_project = if nested {
                let path = root.join("inner.project.json");
                std::fs::write(
                    &path,
                    serde_json::to_vec(&serde_json::json!({ "tree": service })).unwrap(),
                )
                .unwrap();
                path
            } else {
                root_project.clone()
            };
            let job = disk_job(root);
            let mut context = crate::snapshot::InstanceContext::default();
            context.set_reserved_cache_path(job.project().folder_location().join(".rojo"));
            let mut snapshot = snapshot_from_vfs(&context, &job.vfs, &job.project().file_location)
                .unwrap()
                .unwrap();
            let require_context = crate::serve_session::build_require_context(
                &snapshot,
                job.project().folder_location(),
            )
            .unwrap();
            crate::snapshot_middleware::prepare_requires(&mut snapshot, &job.vfs, &require_context)
                .unwrap();
            *job.tree.lock().unwrap() = RojoTree::new(snapshot);
            let app = {
                let tree = job.tree.lock().unwrap();
                tree.descendants(tree.get_root_id())
                    .find(|instance| instance.name() == "App")
                    .unwrap()
                    .id()
            };
            let changed_project = job.vfs.canonicalize(&changed_project).unwrap();
            let project_source = job.vfs.read_to_string(&changed_project).unwrap();
            job.vfs
                .write(
                    &changed_project,
                    project_source.replace("\"Shared\":", "\"RenamedShared\":"),
                )
                .unwrap();
            let luaurc = job.project().folder_location().join(".luaurc");
            job.vfs.write(&luaurc, "{").unwrap();
            assert!(job
                .try_apply_patches_with_publication(changed_project.clone(), |_, _| {})
                .is_err());
            assert_eq!(job.message_queue.cursor(), 0);
            {
                let tree = job.tree.lock().unwrap();
                assert!(tree
                    .descendants(tree.get_root_id())
                    .any(|instance| instance.name() == "Shared"));
                assert!(
                    matches!(&tree.get_instance(app).unwrap().properties()[&ustr("Source")], Variant::String(source) if source.contains("@game/ReplicatedStorage/Shared/Util"))
                );
            }
            job.vfs
                .write(
                    &luaurc,
                    r#"{"aliases":{"shared":"Source/Shared","future":"Source/Shared/Extra.luau"}}"#,
                )
                .unwrap();
            job.apply_patches_with_publication(changed_project, |tree, messages| {
                assert_eq!(messages.len(), 1);
                assert_eq!(job.message_queue.cursor(), 0);
                assert!(tree.descendants(tree.get_root_id()).any(|instance| instance.name() == "RenamedShared"));
                let source = &tree.get_instance(app).unwrap().properties()[&ustr("Source")];
                assert!(matches!(source, Variant::String(source) if source.contains("@game/ReplicatedStorage/RenamedShared/Util")), "{source:?}");
                assert!(messages.iter().any(|message| message.updated.iter().any(|update| {
                    update.id == app && update.changed_properties.contains_key(&ustr("Source"))
                })));
            });
            assert!(job.message_queue.cursor() > 0);
            let app_path = job.project().folder_location().join("App.luau");
            job.vfs
                .write(
                    &app_path,
                    "return require(\"@shared/Util\"), require(\"@future\")\n",
                )
                .unwrap();
            assert!(!event_changes_require_context(&job, &app_path));
            job.apply_patches(app_path);
            assert!(
                matches!(&job.tree.lock().unwrap().get_instance(app).unwrap().properties()[&ustr("Source")], Variant::String(source) if source.contains("/Extra.luau"))
            );
            let added = job
                .project()
                .folder_location()
                .join("Source/Shared/Extra.luau");
            job.vfs
                .write(&added, "return require(\"@shared/Util\")\n")
                .unwrap();
            assert!(event_changes_require_context(&job, &added));
            job.apply_patches(added.clone());
            {
                let tree = job.tree.lock().unwrap();
                for name in ["App", "Extra"] {
                    let instance = tree
                        .descendants(tree.get_root_id())
                        .find(|instance| instance.name() == name)
                        .unwrap();
                    let source = &instance.properties()[&ustr("Source")];
                    assert!(
                        matches!(source, Variant::String(source) if source.contains("@game/ReplicatedStorage/RenamedShared/Util")),
                        "{name} {source:?}"
                    );
                    if name == "App" {
                        assert!(
                            matches!(source, Variant::String(source) if source.contains("@game/ReplicatedStorage/RenamedShared/Extra\"") && !source.contains("/Extra.luau"))
                        );
                    }
                    let transform = instance.metadata().source_transform.as_ref().unwrap();
                    assert!(transform.original.contains("@shared/Util"));
                    assert_eq!(source, &Variant::String(transform.emitted.clone()));
                    assert_eq!(
                        instance
                            .metadata()
                            .context
                            .require_context
                            .as_ref()
                            .unwrap()
                            .path_mappings,
                        tree.root()
                            .metadata()
                            .context
                            .require_context
                            .as_ref()
                            .unwrap()
                            .path_mappings
                    );
                }
            }
            job.vfs.remove_file(&added).unwrap();
            job.apply_patches(added);
            let tree = job.tree.lock().unwrap();
            assert!(!tree
                .descendants(tree.get_root_id())
                .any(|instance| instance.name() == "Extra"));
        }
    }

    #[test]
    fn invalid_package_remount_keeps_the_live_tree_and_revision_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let job = package_relocation_job(temp.path(), true);
        let mut context = job.tree.lock().unwrap().root().metadata().context.clone();
        context.set_reserved_cache_path(job.project().folder_location().join(".rojo"));
        let path = job.project().file_location.clone();
        let mut snapshot = snapshot_from_vfs(&context, &job.vfs, &path)
            .unwrap()
            .unwrap();
        let require_context =
            crate::serve_session::build_require_context(&snapshot, job.project().folder_location())
                .unwrap();
        crate::snapshot_middleware::prepare_requires(&mut snapshot, &job.vfs, &require_context)
            .unwrap();
        *job.tree.lock().unwrap() = RojoTree::new(snapshot);
        let before = job.tree.lock().unwrap().clone_for_serialization();
        job.note_authority(WriteAuthority::Studio);
        job.vfs.write(&path, r#"{"name":"Test","syncbackRules":{"modelTrees":[{"pattern":"App/Left","format":"rbxm"}]},"tree":{"$className":"Folder","App":{"$path":"App"},"First":{"$path":"Packages/_Index/First/library"},"Duplicate":{"$path":"Packages/_Index/First/library"}}}"#).unwrap();
        let disk_before = job.vfs.snapshot_tree(temp.path()).unwrap();
        job.handle_vfs_event(VfsEvent::Write(path));
        assert!(job.project().syncback_rules.is_none());
        assert_eq!(job.vfs.snapshot_tree(temp.path()).unwrap(), disk_before);
        assert_eq!(job.message_queue.cursor(), 0);
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            tree.inner().descendants().count(),
            before.inner().descendants().count()
        );
        for original in before.descendants(before.get_root_id()) {
            let current = tree.get_instance(original.id()).unwrap();
            assert_eq!(current.name(), original.name());
            assert_eq!(current.children(), original.children());
            assert_eq!(current.properties(), original.properties());
            assert_eq!(current.metadata(), original.metadata());
        }
    }

    #[test]
    fn package_batches_report_each_excluded_or_unpersisted_property() {
        for exclusion in ["property", "tree", "path"] {
            let temp = tempfile::tempdir().unwrap();
            let mut job = package_relocation_job(temp.path(), true);
            let context = job.tree.lock().unwrap().root().metadata().context.clone();
            let snapshot = snapshot_from_vfs(
                &context,
                &job.vfs,
                &job.project().folder_location().join("default.project.json"),
            )
            .unwrap()
            .unwrap();
            *job.tree.lock().unwrap() = RojoTree::new(snapshot);
            let id = {
                let tree = job.tree.lock().unwrap();
                tree.descendants(tree.get_root_id())
                    .find(|instance| {
                        instance.name() == "Code"
                            && instance
                                .metadata()
                                .relevant_paths
                                .iter()
                                .any(|path| path.ends_with("Local/First/Left/Code.luau"))
                    })
                    .unwrap()
                    .id()
            };
            let rules = match exclusion {
                "property" => serde_json::json!({
                    "ignoreProperties": { "ModuleScript": ["Archivable"] }
                }),
                "tree" => serde_json::json!({ "ignoreTrees": ["First"] }),
                "path" => serde_json::json!({ "ignorePaths": ["Local/First"] }),
                _ => unreachable!(),
            };
            Arc::get_mut(job.project.project.get_mut().unwrap())
                .unwrap()
                .syncback_rules = Some(serde_json::from_value(rules).unwrap());
            let source = temp.path().join("Local/First/Left/Code.luau");
            let installed = temp.path().join("Packages/_Index/First/library");
            let installed_before = job.vfs.snapshot_tree(&installed).unwrap();
            let mut patch = property_update(id, "Source", Variant::String("return 42\n".into()));
            patch.updated_instances[0]
                .changed_properties
                .insert(ustr("Archivable"), Some(Variant::Bool(false)));
            let (accepted, conflicts, errors) = studio_write(&job, patch);
            assert_eq!(conflicts, PatchSet::new(), "{exclusion}");
            assert!(
                errors.iter().any(|error| {
                    error.id == id && error.property.as_deref() == Some("Archivable")
                }),
                "{exclusion} {errors:?}"
            );
            if exclusion == "property" {
                assert_eq!(
                    accepted,
                    property_update(id, "Source", Variant::String("return 42\n".into()))
                );
                assert_eq!(
                    job.vfs.read_to_string(&source).unwrap().as_str(),
                    "return 42\n"
                );
                assert_eq!(errors.len(), 1, "{errors:?}");
            } else {
                assert_eq!(accepted, PatchSet::new(), "{exclusion}");
                assert!(
                    errors.iter().any(|error| {
                        error.id == id && error.property.as_deref() == Some("Source")
                    }),
                    "{exclusion} {errors:?}"
                );
                assert_eq!(
                    job.vfs.read_to_string(&source).unwrap().as_str(),
                    "return 1\n"
                );
                assert_eq!(errors.len(), 2, "{errors:?}");
            }
            assert_eq!(job.vfs.snapshot_tree(&installed).unwrap(), installed_before);
        }
    }

    fn reload_package_property_fixture(job: &JobThreadContext) {
        let mut context = job.tree.lock().unwrap().root().metadata().context.clone();
        context.set_reserved_cache_path(job.project().folder_location().join(".rojo"));
        let mut snapshot = snapshot_from_vfs(
            &context,
            &job.vfs,
            &job.project().folder_location().join("default.project.json"),
        )
        .unwrap()
        .unwrap();
        let require_context =
            crate::serve_session::build_require_context(&snapshot, job.project().folder_location())
                .unwrap();
        crate::snapshot_middleware::prepare_requires(&mut snapshot, &job.vfs, &require_context)
            .unwrap();
        *job.tree.lock().unwrap() = RojoTree::new(snapshot);
    }

    #[test]
    fn package_property_batch_refreshes_project_defined_child_from_disk() {
        for nested in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let job = package_relocation_job(temp.path(), true);
            let entry = temp.path().join("Local/First/default.project.json");
            let project_path = if nested {
                job.vfs
                    .write(&entry, r#"{"tree":{"$path":"inner.project.json"}}"#)
                    .unwrap();
                temp.path().join("Local/First/inner.project.json")
            } else {
                entry
            };
            let entrance = job
                .vfs
                .canonicalize(temp.path().join("Packages/_Index/First/library"))
                .unwrap();
            job.vfs
            .write(
                &project_path,
                r#"{"tree":{"$className":"Folder","Child":{"$className":"Folder","$properties":{"Archivable":false}}}}"#,
            )
            .unwrap();
            let project_path = job.vfs.canonicalize(&project_path).unwrap();
            reload_package_property_fixture(&job);
            let id = job
                .tree
                .lock()
                .unwrap()
                .inner()
                .descendants()
                .find(|instance| instance.name == "Child")
                .unwrap()
                .referent();
            let (accepted, conflicts, errors) =
                studio_write(&job, property_update(id, "Archivable", Variant::Bool(true)));
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(conflicts, PatchSet::new());
            assert_eq!(accepted.updated_instances.len(), 1);
            assert_eq!(
                accepted.updated_instances[0].changed_properties[&ustr("Archivable")],
                None
            );
            let disk: serde_json::Value =
                serde_json::from_str(&job.vfs.read_to_string(&project_path).unwrap()).unwrap();
            assert_ne!(disk["tree"]["Child"]["$properties"]["Archivable"], false);
            let tree = job.tree.lock().unwrap();
            assert_ne!(
                tree.get_instance(id)
                    .unwrap()
                    .properties()
                    .get(&ustr("Archivable")),
                Some(&Variant::Bool(false))
            );
            let owner = tree
                .get_instance(tree.get_instance(id).unwrap().parent())
                .unwrap();
            assert!(
                tree.get_ids_at_path(&entrance).contains(&owner.id()),
                "{nested}"
            );
            assert_eq!(
                owner.metadata().package_file_contents.get(&project_path),
                Some(&Some(job.vfs.read(&project_path).unwrap().to_vec()))
            );
            drop(tree);
            let (_, conflicts, errors) = studio_write(
                &job,
                property_update(id, "Archivable", Variant::Bool(false)),
            );
            assert_eq!(conflicts, PatchSet::new());
            assert!(errors.is_empty(), "{errors:?}");
        }
    }

    #[test]
    fn package_property_batch_accepts_canonical_source_and_corrects_studio() {
        for authored in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let job = package_relocation_job(temp.path(), true);
            job.vfs
                .write(
                    temp.path().join("Local/First/.luaurc"),
                    r#"{"aliases":{"lib":"."}}"#,
                )
                .unwrap();
            reload_package_property_fixture(&job);
            let id = {
                let tree = job.tree.lock().unwrap();
                tree.descendants(tree.get_root_id())
                    .find(|instance| {
                        instance
                            .metadata()
                            .relevant_paths
                            .iter()
                            .any(|path| path.ends_with("Local/First/Left/Code.luau"))
                    })
                    .unwrap()
                    .id()
            };
            let source = if authored {
                "return require('@lib/Right/Target')\r\n"
            } else {
                "return 42\r\n"
            };
            let cursor = job.message_queue.cursor();
            let (accepted, conflicts, errors) = studio_write(
                &job,
                property_update(id, "Source", Variant::String(source.into())),
            );
            assert!(errors.is_empty(), "{authored} {errors:?}");
            assert_eq!(conflicts, PatchSet::new());
            assert_eq!(accepted.updated_instances.len(), 1);
            let tree = job.tree.lock().unwrap();
            let instance = tree.get_instance(id).unwrap();
            let saved = instance.properties()[&ustr("Source")].clone();
            assert_ne!(saved, Variant::String(source.into()));
            if authored {
                let transform = instance.metadata().source_transform.as_ref().unwrap();
                assert_eq!(transform.original, source.replace("\r\n", "\n"));
                assert_eq!(saved, Variant::String(transform.emitted.clone()));
                assert_ne!(transform.original, transform.emitted);
            } else {
                assert_eq!(saved, Variant::String("return 42\n".into()));
            }
            assert!(messages_since(&job, cursor)
                .iter()
                .flat_map(|message| &message.updated)
                .any(|update| update.id == id
                    && update.changed_properties.get(&ustr("Source"))
                        == Some(&Some(saved.clone()))));
            assert_eq!(
                job.vfs
                    .read_to_string(temp.path().join("Local/First/Left/Code.luau"))
                    .unwrap()
                    .as_str(),
                source
            );
        }
    }

    #[test]
    fn package_property_batch_accepts_numeric_normalization_but_rejects_exclusions() {
        let temp = tempfile::tempdir().unwrap();
        let mut job = package_relocation_job(temp.path(), true);
        job.vfs
            .write(
                temp.path().join("Local/First/Part.model.json"),
                r#"{"ClassName":"Part"}"#,
            )
            .unwrap();
        reload_package_property_fixture(&job);
        let id = job
            .tree
            .lock()
            .unwrap()
            .inner()
            .descendants()
            .find(|instance| instance.name == "Part")
            .unwrap()
            .referent();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules = Some(
            serde_json::from_value(serde_json::json!({
                "ignoreProperties": {"Part": ["Archivable"]}
            }))
            .unwrap(),
        );
        // The second request already matches disk after rounding, but Studio
        // still needs a correction even when refreshing the tree changes nothing.
        for _ in 0..2 {
            let cursor = job.message_queue.cursor();
            let mut patch = property_update(id, "Reflectance", Variant::Float32(0.99999994));
            patch.updated_instances[0]
                .changed_properties
                .insert(ustr("Archivable"), Some(Variant::Bool(true)));
            let (accepted, conflicts, errors) = studio_write(&job, patch);
            assert_eq!(conflicts, PatchSet::new());
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].property.as_deref(), Some("Archivable"));
            assert_eq!(accepted.updated_instances.len(), 1);
            assert!(!accepted.updated_instances[0]
                .changed_properties
                .contains_key(&ustr("Archivable")));
            assert_eq!(
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(id)
                    .unwrap()
                    .properties()[&ustr("Reflectance")],
                Variant::Float32(1.0)
            );
            assert!(messages_since(&job, cursor)
                .iter()
                .flat_map(|message| &message.updated)
                .any(|update| update.id == id
                    && update.changed_properties.get(&ustr("Reflectance"))
                        == Some(&Some(Variant::Float32(1.0)))));
        }
    }

    #[test]
    fn package_batch_serialization_success_preserves_unrelated_live_state() {
        check_package_batch_serialization_preserves_live_state(false);
    }

    #[test]
    fn package_batch_serialization_failure_preserves_live_state() {
        check_package_batch_serialization_preserves_live_state(true);
    }

    fn check_package_batch_serialization_preserves_live_state(fail_serialization: bool) {
        let temp = tempfile::tempdir().unwrap();
        let mut job = package_relocation_job(temp.path(), true);
        // Full-project serialization takes the project root, not the synthetic
        // outer root used by the relocation fixture.
        let context = job.tree.lock().unwrap().root().metadata().context.clone();
        let snapshot = snapshot_from_vfs(
            &context,
            &job.vfs,
            &job.project().folder_location().join("default.project.json"),
        )
        .unwrap()
        .unwrap();
        *job.tree.lock().unwrap() = RojoTree::new(snapshot);
        let find_project = |path: &[&str]| {
            let tree = job.tree.lock().unwrap();
            let mut current = tree.get_root_id();
            for name in path {
                current = *tree
                    .get_instance(current)
                    .unwrap()
                    .children()
                    .iter()
                    .find(|child| tree.get_instance(**child).unwrap().name() == *name)
                    .unwrap();
            }
            current
        };
        let id = find_project(&["First", "Left", "Code"]);
        let unrelated = find_project(&["App", "Right", "Target"]);
        let app = find_project(&["App"]);
        let specified = crate::RojoRef::new("unrelated".into());
        let before = {
            let mut tree = job.tree.lock().unwrap();
            tree.set_specified_id(unrelated, specified.clone());
            tree.get_instance_mut(unrelated)
                .unwrap()
                .properties_mut()
                .extend([
                    (ustr("Archivable"), Variant::Bool(false)),
                    (ustr("Target"), Variant::Ref(id)),
                ]);
            tree.clone_for_serialization()
        };
        let mut rules = serde_json::json!({
            "ignoreProperties": { "ModuleScript": ["Archivable"] }
        });
        if fail_serialization {
            // Model-tree patterns compile after syncback filters both DOMs.
            rules["modelTrees"] = serde_json::json!([{ "pattern": "Maps/[" }]);
        }
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules = Some(serde_json::from_value(rules).unwrap());
        let disk_before = job.vfs.snapshot_tree(temp.path()).unwrap();
        let patch = property_update(id, "Source", Variant::String("return 42\n".into()));
        let (accepted, conflicts, errors) = studio_write(&job, patch.clone());
        assert_eq!(conflicts, PatchSet::new());
        if fail_serialization {
            assert_eq!(accepted, PatchSet::new());
            assert_eq!(errors.len(), 1);
            assert!(errors[0].message.contains("not a valid glob"), "{errors:?}");
            assert_eq!(job.vfs.snapshot_tree(temp.path()).unwrap(), disk_before);
            assert_eq!(job.message_queue.cursor(), 0);
        } else {
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(accepted, patch);
            assert_eq!(
                job.vfs
                    .read_to_string(temp.path().join("Local/First/Left/Code.luau"))
                    .unwrap()
                    .as_str(),
                "return 42\n"
            );
            assert_eq!(
                job.tree.lock().unwrap().get_instance(id).unwrap().name(),
                "Code"
            );
        }
        let tree = job.tree.lock().unwrap();
        assert_eq!(tree.get_root_id(), before.get_root_id());
        // Successful writes refresh the package owner intentionally. Everything
        // outside that owner, or the whole tree on failure, must stay untouched.
        let unchanged_root = if fail_serialization {
            before.get_root_id()
        } else {
            app
        };
        for original in before.descendants(unchanged_root) {
            let live = tree.get_instance(original.id()).unwrap();
            assert_eq!(live.parent(), original.parent());
            assert_eq!(live.children(), original.children());
            assert_eq!(live.name(), original.name());
            assert_eq!(live.class_name(), original.class_name());
            assert_eq!(live.properties(), original.properties());
            assert_eq!(live.metadata(), original.metadata());
            for path in &original.metadata().relevant_paths {
                assert_eq!(tree.get_ids_at_path(path), before.get_ids_at_path(path));
            }
        }
        assert_eq!(tree.get_specified_id(&specified), Some(unrelated));
        assert_eq!(tree.specified_id_holders(&specified), [unrelated]);
        assert_eq!(
            tree.get_instance(id).unwrap().properties()[&ustr("Source")],
            Variant::String(
                if fail_serialization {
                    "return 1\n"
                } else {
                    "return 42\n"
                }
                .into()
            )
        );
    }

    #[test]
    fn package_relocation_ownership_matrix() {
        for installed in [false, true] {
            check_package_relocation_ownership_matrix(installed);
        }
    }

    fn check_package_relocation_ownership_matrix(installed: bool) {
        for source in ["App", "First", "Second"] {
            for destination in ["App", "First", "Second"] {
                for rename in [false, true] {
                    // Exercise unchanged parents, directories, and script promotion.
                    for target in ["Left", "Right", "Right/Target"] {
                        let temp = tempfile::tempdir().unwrap();
                        let job = package_relocation_job(temp.path(), installed);
                        let id = find(&job, &[source, "Left", "Code"]);
                        let destination_path: Vec<_> = std::iter::once(destination)
                            .chain(target.split('/'))
                            .collect();
                        let parent = find(&job, &destination_path);
                        let before = job.vfs.snapshot_tree(temp.path()).unwrap();
                        let patch = PatchSet {
                            updated_instances: vec![PatchUpdate {
                                changed_name: rename.then(|| "Renamed".into()),
                                changed_parent: Some(parent),
                                ..empty_update(id)
                            }],
                            ..PatchSet::new()
                        };
                        let (accepted, conflicts, errors) = studio_write(&job, patch);
                        let case = format!("{source} -> {destination}/{target}, rename {rename}");
                        assert_eq!(conflicts, PatchSet::new(), "{case}");
                        if source != destination {
                            assert!(accepted.updated_instances.is_empty(), "{case}");
                            assert!(
                                errors
                                    .iter()
                                    .any(|error| error.message.contains("ownership")),
                                "{case} {errors:?}"
                            );
                            assert_eq!(
                                job.vfs.snapshot_tree(temp.path()).unwrap(),
                                before,
                                "{case}"
                            );
                            assert_eq!(find(&job, &[source, "Left", "Code"]), id, "{case}");
                            assert_eq!(job.message_queue.cursor(), 0, "{case}");
                        } else {
                            assert!(errors.is_empty(), "{case} {errors:?}");
                            assert_eq!(
                                accepted.updated_instances.len(),
                                usize::from(rename || target != "Left"),
                                "{case}"
                            );
                            let mut path = destination_path;
                            path.push(if rename { "Renamed" } else { "Code" });
                            assert_eq!(find(&job, &path), id, "{case}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn package_relocation_rejects_cross_ownership_before_stale_conflicts() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let temp = tempfile::tempdir().unwrap();
            let job = package_relocation_job(temp.path(), true);
            let id = find(&job, &["First", "Left", "Code"]);
            let parent = find(&job, &["App", "Right"]);
            let before = job.vfs.snapshot_tree(temp.path()).unwrap();
            let (accepted, conflicts, errors) =
                job.handle_tree_event_at_cursor(parent_update(id, parent), None, authority);
            assert_eq!(accepted, PatchSet::new());
            assert_eq!(conflicts, PatchSet::new());
            assert!(
                errors
                    .iter()
                    .any(|error| error.message.contains("ownership")),
                "{errors:?}"
            );
            assert_eq!(job.vfs.snapshot_tree(temp.path()).unwrap(), before);
            assert_eq!(job.message_queue.cursor(), 0);
        }
    }

    #[test]
    fn package_relocation_rejects_cross_ownership_cycles_before_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let job = package_relocation_job(temp.path(), true);
        let local = find(&job, &["First", "Left"]);
        let app = find(&job, &["App", "Right"]);
        let before = job.vfs.snapshot_tree(temp.path()).unwrap();
        let (accepted, conflicts, errors) = studio_write(
            &job,
            PatchSet {
                updated_instances: vec![
                    PatchUpdate {
                        changed_parent: Some(app),
                        ..empty_update(local)
                    },
                    PatchUpdate {
                        changed_parent: Some(local),
                        ..empty_update(app)
                    },
                ],
                ..PatchSet::new()
            },
        );
        assert_eq!(accepted, PatchSet::new());
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(errors.len(), 2);
        assert!(errors
            .iter()
            .all(|error| error.message.contains("ownership")));
        assert_eq!(job.vfs.snapshot_tree(temp.path()).unwrap(), before);
        assert_eq!(job.message_queue.cursor(), 0);
    }

    #[test]
    fn package_relocation_preserves_rename_without_parent_change() {
        for source in ["App", "First", "Second"] {
            let temp = tempfile::tempdir().unwrap();
            let job = package_relocation_job(temp.path(), true);
            let id = find(&job, &[source, "Left", "Code"]);
            let (accepted, conflicts, errors) = studio_write(
                &job,
                PatchSet {
                    updated_instances: vec![PatchUpdate {
                        changed_name: Some("Renamed".into()),
                        ..empty_update(id)
                    }],
                    ..PatchSet::new()
                },
            );
            assert!(errors.is_empty(), "{source} {errors:?}");
            assert_eq!(conflicts, PatchSet::new());
            assert_eq!(accepted.updated_instances.len(), 1);
            assert_eq!(find(&job, &[source, "Left", "Renamed"]), id);
        }
    }

    fn parent_update(id: Ref, parent: Ref) -> PatchSet {
        PatchSet {
            updated_instances: vec![PatchUpdate {
                changed_parent: Some(parent),
                ..empty_update(id)
            }],
            ..PatchSet::new()
        }
    }

    /// Studio sends a descendant and its ancestor in one batch when both are
    /// deleted, in either order. The ancestor's files go, and nothing comes
    /// back, however deeply the deleted descendants are nested.
    #[test]
    fn batches_deleting_descendants_with_their_ancestor_delete_everything() {
        let orders: [&[&[&str]]; 4] = [
            &[&["Old", "Mid", "Leaf"], &["Old"]],
            &[&["Old"], &["Old", "Mid", "Leaf"]],
            &[&["Old", "Mid", "Leaf"], &["Old", "Mid"], &["Old"]],
            &[&["Old", "Mid"], &["Old"], &["Old", "Mid", "Leaf"]],
        ];
        for order in orders {
            let (job, _) = script_job(
                vec![(
                    "src",
                    VfsSnapshot::dir([(
                        "Old",
                        VfsSnapshot::dir([(
                            "Mid",
                            VfsSnapshot::dir([("Leaf.luau", VfsSnapshot::file("return 1"))]),
                        )]),
                    )]),
                )],
                "/project/src",
            );
            let ids: Vec<Ref> = order.iter().map(|path| find(&job, path)).collect();
            let (accepted, conflicts, errors) = studio_write(
                &job,
                PatchSet {
                    removed_instances: ids.clone(),
                    ..PatchSet::new()
                },
            );
            assert!(errors.is_empty(), "{order:?} {errors:?}");
            assert!(conflicts.added_instances.is_empty(), "{order:?}");
            let accepted: HashSet<Ref> = accepted.removed_instances.into_iter().collect();
            assert_eq!(accepted, ids.into_iter().collect(), "{order:?}");
            assert!(job
                .vfs
                .metadata("/project/src/Old")
                .with_not_found()
                .unwrap()
                .is_none());
            let tree = job.tree.lock().unwrap();
            assert!(tree
                .descendants(tree.get_root_id())
                .all(|instance| instance.name() != "Old"));
        }
    }

    #[test]
    fn retained_children_escape_before_ancestor_deletion_and_block_it_on_failure() {
        for collision in [false, true] {
            let (job, root) = script_job(
                vec![(
                    "src",
                    VfsSnapshot::dir([
                        (
                            "Old",
                            VfsSnapshot::dir([("Child.lua", VfsSnapshot::file("return 1"))]),
                        ),
                        ("Other.lua", VfsSnapshot::file("return 2")),
                    ]),
                )],
                "/project/src",
            );
            let old = find(&job, &["Old"]);
            let child = find(&job, &["Old", "Child"]);
            if collision {
                job.vfs.write("/project/src/Child.lua", "occupied").unwrap();
            }
            let mut patch = parent_update(child, root);
            patch.removed_instances.push(old);
            let (accepted, conflicts, errors) = job.handle_tree_event(patch);
            if collision {
                assert!(!errors.is_empty());
                assert!(accepted.removed_instances.is_empty());
                assert!(!conflicts.added_instances.is_empty());
                assert_eq!(find(&job, &["Old", "Child"]), child);
                assert_eq!(
                    read_text(&job.vfs, "/project/src/Old/Child.lua").as_deref(),
                    Some("return 1")
                );
                assert_eq!(
                    read_text(&job.vfs, "/project/src/Child.lua").as_deref(),
                    Some("occupied")
                );
            } else {
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(accepted.removed_instances, vec![old]);
                assert_eq!(conflicts, PatchSet::new());
                assert_eq!(find(&job, &["Child"]), child);
                assert!(!job.vfs.exists("/project/src/Old").unwrap());
                assert_eq!(
                    read_text(&job.vfs, "/project/src/Child.lua").as_deref(),
                    Some("return 1")
                );
            }
        }
    }

    #[test]
    fn relocation_moves_file_sidecars_and_preserves_ids_for_later_writes() {
        let (job, _, _, right) = relocation_job();
        let id = find(&job, &["Left", "Code"]);
        let cursor = job.message_queue.cursor();
        let result = job.handle_tree_event(parent_update(id, right));
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(result.0.updated_instances[0].changed_parent, Some(right));
        assert_eq!(find(&job, &["Right", "Code"]), id);
        assert!(read_text(&job.vfs, "/project/src/Left/Code.server.lua").is_none());
        assert!(read_text(&job.vfs, "/project/src/Right/Code.meta.jsonc")
            .unwrap()
            .contains("Disabled"));
        assert!(messages_since(&job, cursor)
            .iter()
            .all(|patch| patch.removed.is_empty() && patch.added.is_empty()));
        let result = job.handle_tree_event(property_update(
            id,
            "Source",
            Variant::String("print('new')".into()),
        ));
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(
            read_text(&job.vfs, "/project/src/Right/Code.server.lua").unwrap(),
            "print('new')"
        );
        let tree = job.tree.lock().unwrap();
        assert!(tree
            .get_ids_at_path(Path::new("/project/src/Left/Code.server.lua"))
            .is_empty());
        assert_eq!(
            tree.get_ids_at_path(Path::new("/project/src/Right/Code.server.lua")),
            &[id]
        );
        // Path equality ignores a trailing slash, but real filesystems do not.
        assert_eq!(
            tree.get_metadata(id)
                .unwrap()
                .instigating_source
                .as_ref()
                .unwrap()
                .path()
                .as_os_str(),
            Path::new("/project")
                .join("src")
                .join("Right")
                .join("Code.server.lua")
                .as_os_str()
        );
    }

    #[test]
    fn relocation_directory_preserves_unmanaged_contents_and_delayed_watcher_ids() {
        for reverse in [false, true] {
            let (job, _, _, right) = relocation_job();
            let folder = find(&job, &["Left", "Folder"]);
            let child = find(&job, &["Left", "Folder", "Child"]);
            let result = job.handle_tree_event(parent_update(folder, right));
            assert!(result.2.is_empty(), "{result:?}");
            assert_eq!(
                read_text(&job.vfs, "/project/src/Right/Folder/unmanaged.bin").unwrap(),
                "private bytes"
            );
            let mut events = vec![
                VfsEvent::Remove("/project/src/Left/Folder".into()),
                VfsEvent::Create("/project/src/Right/Folder".into()),
                VfsEvent::Write("/project/src/Right/Folder/Child.luau".into()),
            ];
            if reverse {
                events.reverse();
            }
            for event in events {
                job.handle_vfs_event(event);
            }
            assert_eq!(find(&job, &["Right", "Folder"]), folder);
            assert_eq!(find(&job, &["Right", "Folder", "Child"]), child);
            let result = job.handle_tree_event(property_update(
                child,
                "Source",
                Variant::String("return 2".into()),
            ));
            assert!(result.2.is_empty(), "{result:?}");
            assert_eq!(
                read_text(&job.vfs, "/project/src/Right/Folder/Child.luau").unwrap(),
                "return 2"
            );
        }
    }

    #[test]
    fn relocation_rename_clears_a_metadata_name_override_before_accepting_name() {
        let (job, id) = script_job(
            vec![
                ("Physical.server.lua", VfsSnapshot::file("print('kept')")),
                (
                    "Physical.meta.jsonc",
                    VfsSnapshot::file(r#"{"name":"Visible","properties":{"Disabled":true}}"#),
                ),
            ],
            "/project/Physical.server.lua",
        );
        let result = job.handle_tree_event(rename_update(id, "Renamed"));
        assert!(result.2.is_empty(), "{result:?}");
        assert!(result
            .0
            .updated_instances
            .iter()
            .any(|update| update.changed_name.as_deref() == Some("Renamed")));
        assert_eq!(
            read_text(&job.vfs, "/project/Renamed.server.lua").unwrap(),
            "print('kept')"
        );
        let tree = job.tree.lock().unwrap();
        let instance = tree.get_instance(id).unwrap();
        let disk = snapshot_from_vfs(
            &instance.metadata().context,
            &job.vfs,
            Path::new("/project/Renamed.server.lua"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(disk.name, "Renamed");
        assert_eq!(instance.name(), "Renamed");
    }

    #[test]
    fn relocation_rejects_collisions_without_discarding_property_edits() {
        let (job, _, left, right) = relocation_job();
        let id = find(&job, &["Left", "Code"]);
        job.vfs
            .write("/project/src/Right/Code.meta.json", "{}")
            .unwrap();
        let mut update = parent_update(id, right);
        update.updated_instances[0].changed_properties.insert(
            ustr("Source"),
            Some(Variant::String("print('saved')".into())),
        );
        let result = job.handle_tree_event(update);
        assert!(!result.2.is_empty());
        assert!(result
            .0
            .updated_instances
            .iter()
            .any(|update| update.changed_properties.contains_key(&ustr("Source"))));
        assert_eq!(
            job.tree.lock().unwrap().get_instance(id).unwrap().parent(),
            left
        );
        assert_eq!(
            read_text(&job.vfs, "/project/src/Right/Code.meta.json").unwrap(),
            "{}"
        );
        assert_eq!(
            read_text(&job.vfs, "/project/src/Left/Code.server.lua").unwrap(),
            "print('saved')"
        );
    }

    #[test]
    fn relocation_authority_controls_stale_batches_and_never_resurrects_sources() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let (job, _, left, right) = relocation_job();
            let id = find(&job, &["Left", "Code"]);
            job.vfs
                .write("/project/src/Left/Code.server.lua", "print('disk')")
                .unwrap();
            let result = job.handle_tree_event_at_cursor(parent_update(id, right), None, authority);
            assert!(result.2.is_empty(), "{result:?}");
            let parent = job.tree.lock().unwrap().get_instance(id).unwrap().parent();
            if authority == WriteAuthority::Filesystem {
                assert_eq!(parent, left);
                assert!(result
                    .1
                    .updated_instances
                    .iter()
                    .any(|update| update.changed_parent == Some(left)));
            } else {
                assert_eq!(parent, right);
                assert_eq!(
                    read_text(&job.vfs, "/project/src/Right/Code.server.lua").unwrap(),
                    "print('disk')"
                );
            }
            let (job, _, _, right) = relocation_job();
            let id = find(&job, &["Left", "Code"]);
            job.vfs
                .remove_file("/project/src/Left/Code.server.lua")
                .unwrap();
            let result = job.handle_tree_event_at_cursor(parent_update(id, right), None, authority);
            assert!(result.0.updated_instances.is_empty());
            assert!(read_text(&job.vfs, "/project/src/Right/Code.server.lua").is_none());
        }
    }

    #[test]
    fn relocation_allows_existing_project_directory_destinations_but_not_project_sources() {
        let (job, _, _, right) = relocation_job();
        let id = find(&job, &["Left", "Code"]);
        {
            let mut tree = job.tree.lock().unwrap();
            let mut metadata = tree.get_metadata(right).unwrap().clone();
            let mut node = job.project().tree.clone();
            node.path = Some(crate::project::PathNode::Required(
                "/project/src/Right".into(),
            ));
            metadata.instigating_source = Some(InstigatingSource::ProjectNode {
                path: "/project/default.project.json".into(),
                name: "Right".into(),
                node,
                parent_class: None,
            });
            tree.update_metadata(right, metadata);
        }
        let result = job.handle_tree_event(parent_update(id, right));
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(find(&job, &["Right", "Code"]), id);
        let result = job.handle_tree_event(rename_update(right, "Forbidden"));
        assert!(result.0.updated_instances.is_empty());
        assert!(result.2[0].message.contains("Project-defined"));
        assert!(job.vfs.metadata("/project/src/Right").unwrap().is_dir());
    }

    #[test]
    fn relocation_rejects_aliased_sources_and_ignored_destinations() {
        for aliased in [false, true] {
            let (job, root, _, right) = relocation_job();
            let id = find(&job, &["Left", "Code"]);
            {
                let mut tree = job.tree.lock().unwrap();
                if aliased {
                    let snapshot = snapshot_from_tree(&tree, id)
                        .metadata(tree.get_metadata(id).unwrap().clone());
                    tree.insert_instance(root, snapshot);
                } else {
                    let mut metadata = tree.get_metadata(right).unwrap().clone();
                    metadata
                        .context
                        .add_path_ignore_rules([crate::snapshot::PathIgnoreRule {
                            base_path: "/project".into(),
                            glob: crate::glob::IgnorableGlob::new("src/Right/**").unwrap(),
                        }]);
                    tree.update_metadata(right, metadata);
                }
            }
            let result = job.handle_tree_event(parent_update(id, right));
            assert!(result.0.updated_instances.is_empty());
            assert!(!result.2.is_empty());
            assert!(read_text(&job.vfs, "/project/src/Left/Code.server.lua").is_some());
            assert!(read_text(&job.vfs, "/project/src/Right/Code.server.lua").is_none());
        }
    }

    #[test]
    fn relocation_directory_conflicts_restore_full_current_disk_subtrees() {
        let (job, _, left, right) = relocation_job();
        let folder = find(&job, &["Left", "Folder"]);
        let child = find(&job, &["Left", "Folder", "Child"]);
        job.vfs
            .write("/project/src/Left/Folder/New.luau", "return 7")
            .unwrap();
        job.vfs
            .write("/project/src/Left/Folder/Child.luau", "return 9")
            .unwrap();
        let result = job.handle_tree_event(parent_update(folder, right));
        assert!(result.0.updated_instances.is_empty());
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(
            job.tree
                .lock()
                .unwrap()
                .get_instance(folder)
                .unwrap()
                .parent(),
            left
        );
        assert_eq!(find(&job, &["Left", "Folder", "Child"]), child);
        assert!(
            job.tree
                .lock()
                .unwrap()
                .get_instance(child)
                .unwrap()
                .properties()
                .get(&ustr("Source"))
                == Some(&Variant::String("return 9".into()))
        );
        find(&job, &["Left", "Folder", "New"]);
        assert!(!result.1.added_instances.is_empty());
    }

    #[test]
    fn relocation_promotes_script_and_preserves_parent_child_and_descendant_refs() {
        let (job, _, left, _) = relocation_job();
        let folder = find(&job, &["Left", "Folder"]);
        let child = find(&job, &["Left", "Folder", "Child"]);
        let script = find(&job, &["Left", "Code"]);
        let cursor = job.message_queue.cursor();
        let result = job.handle_tree_event(parent_update(folder, script));
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(find(&job, &["Left", "Code"]), script);
        assert_eq!(find(&job, &["Left", "Code", "Folder"]), folder);
        assert_eq!(find(&job, &["Left", "Code", "Folder", "Child"]), child);
        assert!(read_text(&job.vfs, "/project/src/Left/Code.server.lua").is_none());
        assert!(read_text(&job.vfs, "/project/src/Left/Code.meta.jsonc").is_none());
        assert_eq!(
            read_text(&job.vfs, "/project/src/Left/Code/init.server.lua").unwrap(),
            "print('original')"
        );
        assert!(
            read_text(&job.vfs, "/project/src/Left/Code/init.meta.jsonc")
                .unwrap()
                .contains("Disabled")
        );
        assert_eq!(
            read_text(&job.vfs, "/project/src/Left/Code/Folder/unmanaged.bin").unwrap(),
            "private bytes"
        );
        assert!(messages_since(&job, cursor)
            .iter()
            .all(|patch| patch.removed.is_empty() && patch.added.is_empty()));
        for (id, text, path) in [
            (
                script,
                "print('parent edit')",
                "/project/src/Left/Code/init.server.lua",
            ),
            (
                child,
                "return 2",
                "/project/src/Left/Code/Folder/Child.luau",
            ),
        ] {
            let result =
                job.handle_tree_event(property_update(id, "Source", Variant::String(text.into())));
            assert!(result.2.is_empty(), "{result:?}");
            assert_eq!(read_text(&job.vfs, path).unwrap(), text);
        }
        for event in [
            VfsEvent::Remove("/project/src/Left/Code.server.lua".into()),
            VfsEvent::Create("/project/src/Left/Code".into()),
            VfsEvent::Write("/project/src/Left/Code/Folder/Child.luau".into()),
        ] {
            job.handle_vfs_event(event);
        }
        assert_eq!(find(&job, &["Left", "Code", "Folder", "Child"]), child);
        {
            let tree = job.tree.lock().unwrap();
            assert!(tree
                .get_ids_at_path(Path::new("/project/src/Left/Code.server.lua"))
                .is_empty());
            assert!(tree
                .get_ids_at_path(Path::new("/project/src/Left/Code/init.server.lua"))
                .contains(&script));
            assert!(tree
                .get_ids_at_path(Path::new("/project/src/Left/Code/init.meta.jsonc"))
                .contains(&script));
        }
        let result = job.handle_tree_event(parent_update(folder, left));
        assert!(result.2.is_empty(), "{result:?}");
        assert!(job.vfs.metadata("/project/src/Left/Code").unwrap().is_dir());
        assert_eq!(find(&job, &["Left", "Code"]), script);
    }

    #[test]
    fn promotion_moves_file_sidecars_with_combined_rename_and_properties() {
        for suffix in [
            ".lua",
            ".luau",
            ".server.lua",
            ".server.luau",
            ".client.lua",
            ".client.luau",
        ] {
            let filename = format!("Physical{suffix}");
            let (job, _) = script_job(
                vec![(
                    "src",
                    VfsSnapshot::dir([
                        (filename.as_str(), VfsSnapshot::file("-- parent")),
                        (
                            "Physical.meta.json",
                            VfsSnapshot::file(r#"{"name":"Visible","attributes":{"Kept":true}}"#),
                        ),
                        ("Child.server.lua", VfsSnapshot::file("-- child")),
                        (
                            "Child.meta.jsonc",
                            VfsSnapshot::file(r#"{"properties":{"Disabled":true}}"#),
                        ),
                    ]),
                )],
                "/project/src",
            );
            let parent = find(&job, &["Visible"]);
            let child = find(&job, &["Child"]);
            let original_class = job
                .tree
                .lock()
                .unwrap()
                .get_instance(parent)
                .unwrap()
                .class_name();
            let mut patch =
                property_update(child, "Source", Variant::String("-- edited child".into()));
            patch.updated_instances[0].changed_parent = Some(parent);
            patch.updated_instances[0].changed_name = Some("Renamed".into());
            let result = job.handle_tree_event(patch);
            assert!(result.2.is_empty(), "{suffix} {result:?}");
            assert_eq!(find(&job, &["Visible"]), parent);
            assert_eq!(find(&job, &["Visible", "Renamed"]), child);
            assert_eq!(
                job.tree
                    .lock()
                    .unwrap()
                    .get_instance(parent)
                    .unwrap()
                    .class_name(),
                original_class
            );
            let initializer = format!("/project/src/Physical/init{suffix}");
            assert_eq!(read_text(&job.vfs, &initializer).unwrap(), "-- parent");
            assert!(read_text(&job.vfs, "/project/src/Physical/init.meta.json")
                .unwrap()
                .contains("Kept"));
            assert!(
                read_text(&job.vfs, "/project/src/Physical/Renamed.meta.jsonc")
                    .unwrap()
                    .contains("Disabled")
            );
            assert_eq!(
                read_text(&job.vfs, "/project/src/Physical/Renamed.server.lua").unwrap(),
                "-- edited child"
            );
            let result = job.handle_tree_event(property_update(
                parent,
                "Source",
                Variant::String("-- edited parent".into()),
            ));
            assert!(result.2.is_empty(), "{suffix} {result:?}");
            assert_eq!(
                read_text(&job.vfs, &initializer).unwrap(),
                "-- edited parent"
            );
        }
    }

    #[test]
    fn promotion_preserves_script_kind_with_legacy_and_run_context_snapshots() {
        for legacy in [false, true] {
            for suffix in [".server.lua", ".client.lua"] {
                let filename = format!("Parent{suffix}");
                let (job, _) = script_job(
                    vec![(
                        "src",
                        VfsSnapshot::dir([
                            (filename.as_str(), VfsSnapshot::file("-- parent")),
                            ("Child.luau", VfsSnapshot::file("return 1")),
                        ]),
                    )],
                    "/project/src",
                );
                let context =
                    crate::snapshot::InstanceContext::with_emit_legacy_scripts(Some(legacy));
                let snapshot = snapshot_from_vfs(&context, &job.vfs, Path::new("/project/src"))
                    .unwrap()
                    .unwrap();
                *job.tree.lock().unwrap() = RojoTree::new(
                    InstanceSnapshot::new()
                        .name("Root")
                        .children(vec![snapshot]),
                );
                let parent = find(&job, &["Parent"]);
                let child = find(&job, &["Child"]);
                let (class, properties) = {
                    let tree = job.tree.lock().unwrap();
                    let instance = tree.get_instance(parent).unwrap();
                    (instance.class_name(), instance.properties().clone())
                };
                let result = job.handle_tree_event(parent_update(child, parent));
                assert!(result.2.is_empty(), "{legacy} {suffix} {result:?}");
                assert_eq!(find(&job, &["Parent"]), parent);
                assert_eq!(find(&job, &["Parent", "Child"]), child);
                let tree = job.tree.lock().unwrap();
                let instance = tree.get_instance(parent).unwrap();
                assert_eq!(instance.class_name(), class);
                assert_eq!(instance.properties(), &properties);
                assert!(job
                    .vfs
                    .exists(format!("/project/src/Parent/init{suffix}"))
                    .unwrap());
            }
        }
    }

    #[test]
    fn promotion_never_creates_its_visible_output_as_the_transaction_cache() {
        let (job, parent) = script_job(
            vec![
                (".rojo.luau", VfsSnapshot::file("return 1")),
                ("Child.luau", VfsSnapshot::file("return 2")),
            ],
            "/project/.rojo.luau",
        );
        let snapshot = snapshot_from_vfs(
            &Default::default(),
            &job.vfs,
            Path::new("/project/Child.luau"),
        )
        .unwrap()
        .unwrap();
        let child = {
            let mut tree = job.tree.lock().unwrap();
            let root = tree.get_root_id();
            tree.insert_instance(root, snapshot)
        };
        let before = job.vfs.snapshot_tree(Path::new("/project")).unwrap();
        let result = job.handle_tree_event(parent_update(child, parent));
        assert!(!result.2.is_empty(), "{result:?}");
        assert_eq!(
            job.vfs.snapshot_tree(Path::new("/project")).unwrap(),
            before
        );
    }

    #[test]
    fn promotion_preserves_both_existing_sidecar_formats_byte_for_byte() {
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([
                    ("Parent.luau", VfsSnapshot::file("return 1")),
                    ("Parent.meta.json", VfsSnapshot::file("{}")),
                    ("Parent.meta.jsonc", VfsSnapshot::file("{/* parent */}")),
                    ("Child.luau", VfsSnapshot::file("return 2")),
                    ("Child.meta.json", VfsSnapshot::file("{ }")),
                    ("Child.meta.jsonc", VfsSnapshot::file("{/* child */}")),
                ]),
            )],
            "/project/src",
        );
        let parent = find(&job, &["Parent"]);
        let child = find(&job, &["Child"]);
        let result = job.handle_tree_event(parent_update(child, parent));
        assert!(result.2.is_empty(), "{result:?}");
        for (filename, bytes) in [
            ("init.meta.json", "{}"),
            ("init.meta.jsonc", "{/* parent */}"),
            ("Child.meta.json", "{ }"),
            ("Child.meta.jsonc", "{/* child */}"),
        ] {
            assert_eq!(
                read_text(&job.vfs, &format!("/project/src/Parent/{filename}")).unwrap(),
                bytes
            );
        }
        assert_eq!(find(&job, &["Parent", "Child"]), child);
    }

    #[test]
    fn promotion_rejects_project_aliases_custom_mappings_and_stale_destinations() {
        for failure in ["project", "alias", "custom", "stale"] {
            let (job, root, _, _) = relocation_job();
            let script = find(&job, &["Left", "Code"]);
            let child = find(&job, &["Left", "Folder"]);
            {
                let mut tree = job.tree.lock().unwrap();
                let mut metadata = tree.get_metadata(script).unwrap().clone();
                match failure {
                    "project" => {
                        let mut node = job.project().tree.clone();
                        node.path = Some(crate::project::PathNode::Required(
                            "/project/src/Left/Code.server.lua".into(),
                        ));
                        metadata.instigating_source = Some(InstigatingSource::ProjectNode {
                            path: "/project/default.project.json".into(),
                            name: "Code".into(),
                            node,
                            parent_class: None,
                        });
                        tree.update_metadata(script, metadata);
                    }
                    "alias" => {
                        let snapshot = snapshot_from_tree(&tree, script).metadata(metadata);
                        tree.insert_instance(root, snapshot);
                    }
                    "custom" => {
                        let mut rule: crate::snapshot::SyncRule = serde_json::from_str(r#"{"pattern":"**/*.server.lua","use":"serverScript","suffix":".server.lua"}"#).unwrap();
                        rule.base_path = "/project".into();
                        metadata.context.add_sync_rules([rule]);
                        tree.update_metadata(script, metadata);
                    }
                    _ => {
                        job.vfs
                            .write("/project/src/Left/Code.server.lua", "-- external edit")
                            .unwrap();
                    }
                }
            }
            let before = job.vfs.snapshot_tree(Path::new("/project/src")).unwrap();
            let result = job.handle_tree_event(parent_update(child, script));
            assert!(!result.2.is_empty(), "{failure} {result:?}");
            assert_eq!(
                job.vfs.snapshot_tree(Path::new("/project/src")).unwrap(),
                before
            );
        }
    }

    #[test]
    fn promotion_rejects_existing_stems_and_reserved_child_paths_without_changes() {
        for collision in ["directory", "initializer", "metadata"] {
            let (job, _, _, _) = relocation_job();
            let script = find(&job, &["Left", "Code"]);
            let child = find(&job, &["Left", "Folder"]);
            let mut patch = parent_update(child, script);
            match collision {
                "directory" => job.vfs.create_dir("/project/src/Left/Code").unwrap(),
                "initializer" => {
                    patch.updated_instances[0].changed_name = Some("init.server.lua".into())
                }
                _ => patch.updated_instances[0].changed_name = Some("init.meta.jsonc".into()),
            }
            let before = job.vfs.snapshot_tree(Path::new("/project/src")).unwrap();
            let result = job.handle_tree_event(patch);
            assert!(!result.2.is_empty(), "{collision} {result:?}");
            assert_eq!(
                job.vfs.snapshot_tree(Path::new("/project/src")).unwrap(),
                before
            );
            assert_eq!(find(&job, &["Left", "Code"]), script);
            assert_eq!(find(&job, &["Left", "Folder"]), child);
        }
    }

    #[test]
    fn relocation_directory_rename_preserves_inline_and_file_backed_descendants() {
        let (job, _, _, _) = relocation_job();
        job.vfs
            .write(
                "/project/src/Left/Folder/init.meta.json",
                r#"{"children":[{"name":"Inline","className":"Part"}]}"#,
            )
            .unwrap();
        job.handle_vfs_event(VfsEvent::Write(
            "/project/src/Left/Folder/init.meta.json".into(),
        ));
        let folder = find(&job, &["Left", "Folder"]);
        let child = find(&job, &["Left", "Folder", "Child"]);
        let inline = find(&job, &["Left", "Folder", "Inline"]);
        let result = job.handle_tree_event(rename_update(folder, "Renamed"));
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(find(&job, &["Left", "Renamed"]), folder);
        assert_eq!(find(&job, &["Left", "Renamed", "Child"]), child);
        assert_eq!(find(&job, &["Left", "Renamed", "Inline"]), inline);
        let result = job.handle_tree_event(transparency_update(inline, 0.5));
        assert!(result.2.is_empty(), "{result:?}");
        assert!(
            read_text(&job.vfs, "/project/src/Left/Renamed/init.meta.json")
                .unwrap()
                .contains("Transparency")
        );
    }

    #[test]
    fn relocation_batch_reverses_ancestry_without_a_transient_cycle() {
        let (job, root, left, _) = relocation_job();
        let folder = find(&job, &["Left", "Folder"]);
        let child = find(&job, &["Left", "Folder", "Child"]);
        let result = job.handle_tree_event(PatchSet {
            updated_instances: vec![
                PatchUpdate {
                    changed_parent: Some(folder),
                    ..empty_update(left)
                },
                PatchUpdate {
                    changed_parent: Some(root),
                    ..empty_update(folder)
                },
            ],
            ..PatchSet::new()
        });
        assert!(result.2.is_empty(), "{result:?}");
        assert_eq!(find(&job, &["Folder", "Left"]), left);
        assert_eq!(find(&job, &["Folder", "Child"]), child);
    }

    #[test]
    fn relocation_batch_rejects_final_cycles_before_moving_any_sources() {
        let (job, _, left, right) = relocation_job();
        let result = job.handle_tree_event(PatchSet {
            updated_instances: vec![
                PatchUpdate {
                    changed_parent: Some(right),
                    ..empty_update(left)
                },
                PatchUpdate {
                    changed_parent: Some(left),
                    ..empty_update(right)
                },
            ],
            ..PatchSet::new()
        });
        assert_eq!(result.2.len(), 2);
        assert!(result.0.updated_instances.is_empty());
        assert!(job.vfs.metadata("/project/src/Left").unwrap().is_dir());
        assert!(job.vfs.metadata("/project/src/Right").unwrap().is_dir());
    }

    /// Directory scripts keep the initializer they already use. A metadata-only
    /// edit writes only the metadata, and a Source edit writes only the tracked
    /// `init.server.lua`, never a second `init.server.luau`.
    #[test]
    fn directory_script_writes_keep_the_tracked_initializer() {
        let files = || {
            vec![(
                "Saver",
                VfsSnapshot::dir([
                    ("init.server.lua", VfsSnapshot::file("print('saver')")),
                    (
                        "init.meta.json",
                        VfsSnapshot::file(r#"{"properties":{"Disabled":true}}"#),
                    ),
                ]),
            )]
        };

        let (job, id) = script_job(files(), "/project/Saver");
        let (accepted, _, errors) =
            job.handle_tree_event(property_update(id, "Disabled", Variant::Bool(false)));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(accepted.updated_instances.len(), 1);
        let meta = read_text(&job.vfs, "/project/Saver/init.meta.json").unwrap();
        assert!(!meta.contains("Disabled"), "{meta}");
        assert_eq!(
            read_text(&job.vfs, "/project/Saver/init.server.lua").as_deref(),
            Some("print('saver')")
        );
        assert_eq!(read_text(&job.vfs, "/project/Saver/init.server.luau"), None);

        let (job, id) = script_job(files(), "/project/Saver");
        let (accepted, _, errors) = job.handle_tree_event(property_update(
            id,
            "Source",
            Variant::String("print(1)".into()),
        ));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(accepted.updated_instances.len(), 1);
        assert_eq!(
            read_text(&job.vfs, "/project/Saver/init.server.lua").as_deref(),
            Some("print(1)")
        );
        assert_eq!(read_text(&job.vfs, "/project/Saver/init.server.luau"), None);
    }

    /// Adjacent JSONC metadata is named after the script's stem, so edits go to
    /// `Foo.meta.jsonc` rather than a competing `Foo.meta.json`. Clearing its
    /// last value rewrites it instead of writing nothing and reporting Saved.
    #[test]
    fn adjacent_jsonc_metadata_is_rewritten_when_its_last_value_clears() {
        let (job, id) = script_job(
            vec![
                ("Foo.server.luau", VfsSnapshot::file("print('foo')")),
                (
                    "Foo.meta.jsonc",
                    VfsSnapshot::file(
                        "{\n  // kept as JSONC\n  \"properties\": {\"Disabled\": true}\n}",
                    ),
                ),
            ],
            "/project/Foo.server.luau",
        );

        let (accepted, conflicts, errors) =
            job.handle_tree_event(property_update(id, "Disabled", Variant::Bool(false)));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 1);
        let meta = read_text(&job.vfs, "/project/Foo.meta.jsonc").unwrap();
        assert!(!meta.contains("Disabled"), "{meta}");
        assert_eq!(read_text(&job.vfs, "/project/Foo.meta.json"), None);
        assert_eq!(
            read_text(&job.vfs, "/project/Foo.server.luau").as_deref(),
            Some("print('foo')")
        );
        let reloaded = snapshot_from_vfs(
            &Default::default(),
            &job.vfs,
            Path::new("/project/Foo.server.luau"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(reloaded.properties.get(&ustr("Disabled")), None);
    }

    /// Writing a folder's own properties reads only its own metadata. A child
    /// whose file can't be parsed proves the child isn't read, and the child
    /// must survive the write instead of being treated as removed.
    #[test]
    fn folder_writes_do_not_read_or_remove_children_with_their_own_files() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
                ),
                (
                    "Zone",
                    VfsSnapshot::dir([
                        (
                            "init.meta.json",
                            VfsSnapshot::file(
                                r#"{"className":"Part","properties":{"Transparency":0.25}}"#,
                            ),
                        ),
                        (
                            "Child",
                            VfsSnapshot::dir([(
                                "init.meta.json",
                                VfsSnapshot::file(r#"{"className":"Folder"}"#),
                            )]),
                        ),
                    ]),
                ),
            ]),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let zone = snapshot_from_vfs(&Default::default(), &vfs, Path::new("/project/Zone"))
            .unwrap()
            .unwrap();
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![zone]));
        let zone_id = tree.root().children()[0];
        let child_id = tree.get_instance(zone_id).unwrap().children()[0];
        vfs.write("/project/Zone/Child/init.meta.json", "{ not json")
            .unwrap();
        let job = JobThreadContext {
            tree: Arc::new(Mutex::new(tree)),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };

        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id: zone_id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    ustr("Transparency"),
                    Some(Variant::Float32(0.75)),
                )]),
                changed_metadata: None,
            }],
        });

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 1);
        let contents =
            String::from_utf8(vfs.read("/project/Zone/init.meta.json").unwrap().to_vec()).unwrap();
        assert!(contents.contains("0.75"), "{contents}");
        let tree = job.tree.lock().unwrap();
        assert_eq!(tree.get_instance(zone_id).unwrap().children(), [child_id]);
    }

    /// An edit to a folder's own init.meta.json refreshes only that folder, so a
    /// child whose file can't be parsed isn't read and stays in the tree, while
    /// a new child folder appearing still refreshes the folder fully.
    #[test]
    fn own_file_events_refresh_only_the_instance() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
                ),
                (
                    "Zone",
                    VfsSnapshot::dir([
                        (
                            "init.meta.json",
                            VfsSnapshot::file(
                                r#"{"className":"Part","properties":{"Transparency":0.25}}"#,
                            ),
                        ),
                        (
                            "Child",
                            VfsSnapshot::dir([(
                                "init.meta.json",
                                VfsSnapshot::file(r#"{"className":"Folder"}"#),
                            )]),
                        ),
                    ]),
                ),
            ]),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let zone = snapshot_from_vfs(&Default::default(), &vfs, Path::new("/project/Zone"))
            .unwrap()
            .unwrap();
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root").children(vec![zone]));
        let zone_id = tree.root().children()[0];
        let child_id = tree.get_instance(zone_id).unwrap().children()[0];
        let job = JobThreadContext {
            tree: Arc::new(Mutex::new(tree)),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };

        vfs.write("/project/Zone/Child/init.meta.json", "{ not json")
            .unwrap();
        vfs.write(
            "/project/Zone/init.meta.json",
            r#"{"className":"Part","properties":{"Transparency":0.5}}"#,
        )
        .unwrap();
        job.apply_patches(PathBuf::from("/project/Zone/init.meta.json"));
        {
            let tree = job.tree.lock().unwrap();
            let zone = tree.get_instance(zone_id).unwrap();
            assert_eq!(
                zone.properties().get(&ustr("Transparency")),
                Some(&Variant::Float32(0.5))
            );
            assert_eq!(zone.children(), [child_id]);
        }

        vfs.write(
            "/project/Zone/Child/init.meta.json",
            r#"{"className":"Folder"}"#,
        )
        .unwrap();
        vfs.create_dir("/project/Zone/Added").unwrap();
        job.apply_patches(PathBuf::from("/project/Zone/Added"));
        let tree = job.tree.lock().unwrap();
        let names: Vec<_> = tree
            .get_instance(zone_id)
            .unwrap()
            .children()
            .iter()
            .map(|id| tree.get_instance(*id).unwrap().name().to_owned())
            .collect();
        assert!(names.contains(&"Added".to_owned()), "{names:?}");
        assert!(names.contains(&"Child".to_owned()), "{names:?}");
    }

    /// Studio renames the DataModel to the project name when a session starts.
    /// Writing that back must be rejected without re-snapshotting the project,
    /// which for a large place blocks every other write until it times out.
    #[test]
    fn project_root_edits_are_rejected_without_touching_files() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(r#"{"name":"Test","tree":{"$path":"src"}}"#),
                ),
                (
                    "src",
                    VfsSnapshot::dir([(
                        "Part",
                        VfsSnapshot::dir([(
                            "init.meta.json",
                            VfsSnapshot::file(r#"{"className":"Part"}"#),
                        )]),
                    )]),
                ),
            ]),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let root = snapshot_from_vfs(&Default::default(), &vfs, Path::new("/project"))
            .unwrap()
            .unwrap();
        assert_eq!(root.metadata.middleware, Some(Middleware::Project));
        let tree = RojoTree::new(root);
        let root_id = tree.get_root_id();
        let job = JobThreadContext {
            tree: Arc::new(Mutex::new(tree)),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };

        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id: root_id,
                changed_name: Some("Renamed".into()),
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            }],
        });

        assert_eq!(accepted, PatchSet::new());
        assert_eq!(
            conflicts.updated_instances[0].changed_name.as_deref(),
            Some("Test")
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("tree root"));
        assert_eq!(
            &*vfs.read("/project/default.project.json").unwrap(),
            br#"{"name":"Test","tree":{"$path":"src"}}"#
        );
    }

    #[test]
    fn revalidation_conflicts_use_real_ids_and_restore_requested_values() {
        let (job, id) = part_job();
        let context = Default::default();
        let path = PathBuf::from("/project/Part");
        let baseline = snapshot_from_vfs(&context, &job.vfs, &path).unwrap();
        job.vfs
            .write(
                "/project/Part/init.meta.json",
                r#"{"className":"Part","properties":{"Transparency":0.5,"CanCollide":false}}"#,
            )
            .unwrap();
        job.vfs
            .write("/project/Part/Child.luau", "return 1")
            .unwrap();
        let latest = snapshot_from_vfs(&context, &job.vfs, &path).unwrap();
        let mut update = transparency_update(id, 0.75).updated_instances.remove(0);
        update
            .changed_properties
            .insert(ustr("CanCollide"), Some(Variant::Bool(true)));
        let mut tree = job.tree.lock().unwrap();
        assert!(snapshots_differ(baseline.clone(), latest.as_ref()));
        let patch = disk_restoration(&tree, id, &update, latest);
        assert!(patch.updated_instances.iter().all(|update| update.id == id));
        assert_eq!(patch.added_instances.len(), 1);
        assert_eq!(patch.added_instances[0].parent_id, id);
        assert_eq!(
            patch.updated_instances.last().unwrap().changed_properties[&ustr("CanCollide")],
            Some(Variant::Bool(false))
        );
        apply_patch_set(&mut tree, patch);
        assert_eq!(
            tree.get_instance(id).unwrap().properties()[&ustr("Transparency")],
            Variant::Float32(0.5)
        );
        assert!(snapshots_differ(baseline, None));
        let patch = disk_restoration(&tree, id, &update, None);
        assert_eq!(patch.removed_instances, vec![id]);
    }

    #[test]
    fn missing_and_future_cursors_restore_disk_without_writing() {
        for cursor in [None, Some(u32::MAX)] {
            let (job, id) = part_job();
            job.vfs
                .write(
                    "/project/Part/init.meta.json",
                    r#"{"className":"Part","properties":{"Transparency":0.5,"CanCollide":false}}"#,
                )
                .unwrap();
            job.handle_vfs_event(VfsEvent::Write(PathBuf::from(
                "/project/Part/init.meta.json",
            )));
            assert_ne!(cursor, Some(job.message_queue.cursor()));
            let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                transparency_update(id, 0.75),
                cursor,
                WriteAuthority::Filesystem,
            );
            assert_eq!(accepted, PatchSet::new());
            assert!(errors.is_empty());
            assert_eq!(
                conflicts.updated_instances[0].changed_properties[&ustr("Transparency")],
                Some(Variant::Float32(0.5))
            );
            assert_eq!(
                job.vfs.read("/project/Part/init.meta.json").unwrap(),
                before
            );
        }
    }

    #[test]
    fn stale_deletion_restores_an_unchanged_disk_subtree() {
        let (job, id) = part_job();
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
            PatchSet {
                removed_instances: vec![id],
                ..PatchSet::new()
            },
            None,
            WriteAuthority::Filesystem,
        );
        assert!(accepted.removed_instances.is_empty());
        assert_eq!(conflicts.added_instances.len(), 1);
        assert!(errors.is_empty());
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
        assert_eq!(job.tree.lock().unwrap().root().children(), &[id]);
    }

    #[test]
    fn serialization_failure_restores_requested_studio_values() {
        let (mut job, id) = part_job();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules =
            Some(serde_json::from_value(serde_json::json!({"ignorePaths":["Part/**"]})).unwrap());
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        assert_eq!(
            conflicts.updated_instances[0].changed_properties[&ustr("Transparency")],
            Some(Variant::Float32(0.25))
        );
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
    }

    /// Studio writes with NaN or infinity inside a composite value are saved in
    /// the tagged form, and sending the same values again matches the disk
    /// instead of conflicting, since NaN must compare equal to itself.
    #[test]
    fn non_finite_composite_writes_are_saved_and_stay_in_sync() {
        let (job, id) = part_job();
        let mut attributes = Attributes::new();
        attributes.insert("Range".into(), NumberRange::new(0.0, f32::INFINITY).into());
        attributes.insert(
            "Offset".into(),
            Vector3::new(f32::NAN, 1.0, f32::NEG_INFINITY).into(),
        );
        let pivot = CFrame::new(Vector3::new(1.0, f32::INFINITY, 3.0), Matrix3::identity());
        for attempt in 0..2 {
            let mut patch =
                property_update(id, "Attributes", Variant::Attributes(attributes.clone()));
            patch.updated_instances[0]
                .changed_properties
                .insert(ustr("PivotOffset"), Some(pivot.into()));
            let (accepted, conflicts, errors) = job.handle_tree_event(patch);
            assert!(errors.is_empty(), "attempt {attempt}: {errors:?}");
            assert_eq!(conflicts, PatchSet::new(), "attempt {attempt}");
            // The repeat is already on disk, so there's nothing left to save.
            let expected = if attempt == 0 { 1 } else { 0 };
            assert_eq!(
                accepted.updated_instances.len(),
                expected,
                "attempt {attempt}"
            );
        }
        let meta: serde_json::Value =
            serde_json::from_slice(&job.vfs.read("/project/Part/init.meta.json").unwrap()).unwrap();
        assert_eq!(
            meta["attributes"]["Range"],
            serde_json::json!({"NumberRange": [0, "inf"]})
        );
        assert_eq!(
            meta["attributes"]["Offset"],
            serde_json::json!({"Vector3": ["nan", 1, "-inf"]})
        );
        assert_eq!(
            meta["properties"]["PivotOffset"]["CFrame"]["position"],
            serde_json::json!([1, "inf", 3])
        );
    }

    #[test]
    fn reflection_excluded_properties_are_not_acknowledged_as_saved() {
        let (job, id) = part_job();
        let mut patch = transparency_update(id, 0.75);
        patch.updated_instances[0].changed_properties =
            UstrMap::from_iter([(ustr("AssemblyMass"), Some(Variant::Float32(123.0)))]);
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(patch);
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].property.as_deref(), Some("AssemblyMass"));
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
    }

    #[test]
    fn unscriptable_properties_are_restored_when_disabled() {
        use rbx_reflection::{PropertyKind, PropertySerialization, Scriptability};

        let (mut job, id) = part_job();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules =
            Some(serde_json::from_value(serde_json::json!({"syncUnscriptable":false})).unwrap());
        let database = rbx_reflection_database::get().unwrap();
        let mut class_name = "Part";
        let mut names = Vec::new();
        while let Some(class) = database.classes.get(class_name) {
            for (name, property) in &class.properties {
                if matches!(property.scriptability, Scriptability::None)
                    && matches!(property.kind, PropertyKind::Canonical { ref serialization }
                        if !matches!(serialization, PropertySerialization::DoesNotSerialize))
                {
                    names.push(*name);
                }
            }
            match class.superclass {
                Some(parent) => class_name = parent,
                None => break,
            }
        }
        names.sort_unstable();
        assert!(!names.is_empty());
        let property = ustr(names[0]);
        let mut patch = transparency_update(id, 0.75);
        patch.updated_instances[0].changed_properties =
            UstrMap::from_iter([(property, Some(Variant::String("Studio value".into())))]);
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(patch);
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].property.as_deref(), Some(property.as_str()));
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
    }

    #[test]
    fn unsupported_class_changes_restore_the_disk_class() {
        let (job, id) = part_job();
        let mut patch = transparency_update(id, 0.75);
        patch.updated_instances[0].changed_class_name = Some(ustr("Folder"));
        let (accepted, conflicts, errors) = job.handle_tree_event(patch);
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        assert_eq!(
            conflicts.updated_instances[0].changed_class_name,
            Some(ustr("Part"))
        );
    }

    #[test]
    fn deleting_a_directory_preserves_unrepresented_files() {
        let (job, id) = part_job();
        job.vfs
            .write("/project/Part/notes.txt", "keep this file")
            .unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: vec![id],
            ..PatchSet::new()
        });
        assert!(accepted.removed_instances.is_empty());
        assert_eq!(conflicts.added_instances.len(), 1);
        assert!(errors.is_empty());
        assert_eq!(
            &**job.vfs.read("/project/Part/notes.txt").unwrap(),
            b"keep this file"
        );
        assert_eq!(job.tree.lock().unwrap().root().children(), &[id]);
    }

    #[test]
    fn delete_conflict_preserves_server_ids_without_duplicate_instances() {
        let (job, id) = part_job();
        job.vfs
            .write(
                "/project/Part/init.meta.json",
                r#"{"className":"Part","properties":{"Transparency":0.5,"CanCollide":false}}"#,
            )
            .unwrap();
        let cursor = job.message_queue.cursor();
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: vec![id],
            ..PatchSet::new()
        });
        assert!(accepted.removed_instances.is_empty());
        assert_eq!(conflicts.added_instances.len(), 1);
        assert!(errors.is_empty());
        let tree = job.tree.lock().unwrap();
        assert_eq!(tree.root().children(), &[id]);
        assert_eq!(
            tree.get_instance(id).unwrap().properties()[&ustr("Transparency")],
            Variant::Float32(0.5)
        );
        drop(tree);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (_, messages) = runtime
            .block_on(job.message_queue.subscribe(cursor))
            .unwrap();
        assert_eq!(messages[0].removed, vec![id]);
        assert_eq!(messages[0].added, vec![id]);
    }

    #[test]
    fn pending_disk_properties_survive_a_refused_ordinary_write() {
        let (mut job, id) = part_job();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules =
            Some(serde_json::from_value(serde_json::json!({"ignorePaths":["Part/**"]})).unwrap());
        job.vfs
            .write(
                "/project/Part/init.meta.json",
                r#"{"className":"Part","properties":{"Transparency":0.5,"CanCollide":false}}"#,
            )
            .unwrap();
        let mut patch = transparency_update(id, 0.75);
        patch.updated_instances[0]
            .changed_properties
            .insert(ustr("CanCollide"), Some(Variant::Bool(true)));
        let (accepted, conflicts, errors) = job.handle_tree_event(patch);
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        assert_eq!(
            conflicts.updated_instances[0].changed_properties[&ustr("Transparency")],
            Some(Variant::Float32(0.5))
        );
        assert_eq!(
            job.tree
                .lock()
                .unwrap()
                .get_instance(id)
                .unwrap()
                .properties()[&ustr("Transparency")],
            Variant::Float32(0.5)
        );
        assert!(job
            .vfs
            .read_to_string("/project/Part/init.meta.json")
            .unwrap()
            .contains("false"));
    }

    #[test]
    fn property_write_preserves_an_independent_disk_rename() {
        let (job, id) = part_job();
        job.vfs.write(
            "/project/Part/init.meta.json",
            r#"{"className":"Part","name":"DiskPart","properties":{"Transparency":0.25,"CanCollide":false}}"#,
        ).unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert!(errors.is_empty());
        let contents = job
            .vfs
            .read_to_string("/project/Part/init.meta.json")
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(metadata["name"], "DiskPart");
        assert_eq!(metadata["properties"]["Transparency"], 0.25);
    }

    #[test]
    fn studio_rename_does_not_resurrect_a_deleted_filesystem_source() {
        let (job, id) = part_job();
        job.vfs.lock().remove_dir_all("/project/Part").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(rename_update(id, "Renamed"));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(conflicts.removed_instances, vec![id]);
        assert!(errors.is_empty());
        assert!(job.tree.lock().unwrap().root().children().is_empty());
        assert!(job
            .vfs
            .metadata("/project/Part")
            .with_not_found()
            .unwrap()
            .is_none());
    }

    #[test]
    fn scoped_write_respects_full_ignored_tree_path() {
        let (mut job, id) = part_job();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules =
            Some(serde_json::from_value(serde_json::json!({"ignoreTrees":["Part"]})).unwrap());
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, _, errors) = job.handle_tree_event(transparency_update(id, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
    }

    /// Ignoring only an instance's metadata file must still stop a property
    /// edit, since the write would land in that file.
    #[test]
    fn scoped_write_rejects_an_ignored_metadata_target() {
        let (mut job, id) = part_job();
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules = Some(
            serde_json::from_value(serde_json::json!({"ignorePaths":["Part/init.meta.json"]}))
                .unwrap(),
        );
        let before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("excluded"), "{errors:?}");
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            before
        );
    }

    /// Both metadata files apply when read, so rewriting one can't reliably
    /// change a value and the write is rejected without touching either.
    #[test]
    fn scoped_write_rejects_combined_json_and_jsonc_metadata() {
        let (job, id) = part_job();
        job.vfs
            // Repeats a value the tree already holds, so the disk still matches it.
            .write(
                "/project/Part/init.meta.jsonc",
                br#"{"properties":{"CanCollide":false}}"#,
            )
            .unwrap();
        let json_before = job.vfs.read("/project/Part/init.meta.json").unwrap();
        let jsonc_before = job.vfs.read("/project/Part/init.meta.jsonc").unwrap();
        let (accepted, _, errors) = job.handle_tree_event(transparency_update(id, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("Both"), "{errors:?}");
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.json").unwrap(),
            json_before
        );
        assert_eq!(
            job.vfs.read("/project/Part/init.meta.jsonc").unwrap(),
            jsonc_before
        );
    }

    const INLINE_META: &str = r#"{
        "className": "Part",
        "properties": { "Transparency": 0.125 },
        "children": [
            {
                "className": "Part",
                "name": "Base",
                "properties": { "Transparency": 0.25 },
                "attributes": { "Rojo_Id": "base-id" }
            },
            {
                "className": "Folder",
                "name": "Nested",
                "children": [
                    { "className": "Part", "name": "Deep", "properties": { "Transparency": 0.5 } }
                ]
            },
            {
                "className": "ObjectValue",
                "name": "Pointer",
                "attributes": { "Rojo_Target_Value": "base-id" }
            },
            { "className": "Part", "name": "Sibling", "id": "sibling-id" },
            {
                "className": "ModuleScript",
                "name": "InlineModule",
                "properties": { "Source": "return 1" }
            }
        ]
    }"#;

    /// A directory that stores most of its subtree inline, next to a model file
    /// child that keeps a file of its own.
    fn inline_job() -> (JobThreadContext, Ref) {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project/default.project.json",
            VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
        )
        .unwrap();
        imfs.load_snapshot("/project/Props", VfsSnapshot::empty_dir())
            .unwrap();
        imfs.load_snapshot(
            "/project/Props/init.meta.json",
            VfsSnapshot::file(INLINE_META),
        )
        .unwrap();
        imfs.load_snapshot(
            "/project/Props/Extra.model.json",
            VfsSnapshot::file(
                r#"{"className":"Folder","children":[{"className":"Part","name":"ModelChild"}]}"#,
            ),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let child = snapshot_from_vfs(&Default::default(), &vfs, &PathBuf::from("/project/Props"))
            .unwrap()
            .unwrap();
        // Patch application resolves the attribute references between instances.
        let mut tree = RojoTree::new(InstanceSnapshot::new().name("Root"));
        let root = tree.get_root_id();
        let patch = compute_patch_set(
            Some(InstanceSnapshot::new().name("Root").children(vec![child])),
            &tree,
            root,
        );
        apply_patch_set(&mut tree, patch);
        let id = tree.root().children()[0];
        (
            JobThreadContext {
                tree: Arc::new(Mutex::new(tree)),
                vfs,
                message_queue: Arc::new(MessageQueue::new()),
                project: Arc::new(project).into(),
            },
            id,
        )
    }

    fn messages_since(job: &JobThreadContext, cursor: u32) -> Vec<AppliedPatchSet> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(job.message_queue.subscribe(cursor))
            .map(|(_, messages)| messages)
            .unwrap_or_default()
    }

    fn studio_write(job: &JobThreadContext, patch: PatchSet) -> WriteResult {
        job.handle_tree_event_at_cursor(
            patch,
            Some(job.message_queue.cursor()),
            WriteAuthority::Studio,
        )
    }

    /// Walks every combination of Studio editing the name and a property, the
    /// disk changing the name, that property, and another one, and the disk
    /// agreeing with Studio's values. Only values Studio will actually change
    /// are held back from Studio, and everything else is sent.
    #[test]
    fn studio_edits_are_split_from_the_disk_changes_studio_still_needs() {
        let id = Ref::new();
        let [edited, other] = [ustr("Transparency"), ustr("CanCollide")];
        for bits in 0..64u32 {
            let [studio_name, studio_property, disk_name, disk_property, disk_other, agree] =
                [0, 1, 2, 3, 4, 5].map(|bit| bits & (1 << bit) != 0);
            let update = PatchUpdate {
                id,
                changed_name: studio_name.then(|| "StudioName".to_owned()),
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter(
                    studio_property.then_some((edited, Some(Variant::Float32(0.75)))),
                ),
                changed_metadata: None,
            };
            let (disk_name_value, disk_value) = if agree {
                ("StudioName", 0.75)
            } else {
                ("DiskName", 0.5)
            };
            let mut disk_properties = UstrMap::new();
            if disk_property {
                disk_properties.insert(edited, Some(Variant::Float32(disk_value)));
            }
            if disk_other {
                disk_properties.insert(other, Some(Variant::Bool(true)));
            }
            let disk = PatchSet {
                updated_instances: vec![PatchUpdate {
                    id,
                    changed_name: disk_name.then(|| disk_name_value.to_owned()),
                    changed_class_name: None,
                    changed_parent: None,
                    changed_properties: disk_properties,
                    changed_metadata: None,
                }],
                ..PatchSet::new()
            };
            let (to_studio, overwritten) = split_studio_edits(disk, &[update]);
            let sent = to_studio.updated_instances.first();
            let held = overwritten.updated_instances.first();
            let case = format!("case {bits:06b}");

            let sends_name = disk_name && (!studio_name || agree);
            let sends_edited = disk_property && (!studio_property || agree);
            assert_eq!(
                sent.is_some(),
                sends_name || sends_edited || disk_other,
                "{case}"
            );
            if let Some(sent) = sent {
                assert_eq!(sent.changed_name.is_some(), sends_name, "{case}");
                assert_eq!(
                    sent.changed_properties.contains_key(&edited),
                    sends_edited,
                    "{case}"
                );
                assert_eq!(
                    sent.changed_properties.contains_key(&other),
                    disk_other,
                    "{case}"
                );
            }

            let holds_name = disk_name && studio_name && !agree;
            let holds_edited = disk_property && studio_property && !agree;
            assert_eq!(held.is_some(), holds_name || holds_edited, "{case}");
            if let Some(held) = held {
                assert_eq!(held.changed_name.is_some(), holds_name, "{case}");
                assert_eq!(
                    held.changed_properties.contains_key(&edited),
                    holds_edited,
                    "{case}"
                );
                assert!(!held.changed_properties.contains_key(&other), "{case}");
            }
        }
    }

    /// Two inline siblings share one file, and Studio edits both in a batch
    /// while the disk changed both. Studio is never sent either disk value,
    /// even while the first sibling's write catches up for the second.
    #[test]
    fn studio_authority_holds_back_every_edit_in_the_batch() {
        let (job, owner) = inline_job();
        let base = find(&job, &["Base"]);
        let deep = find(&job, &["Nested", "Deep"]);
        let mut meta = read_props_meta(&job);
        meta["children"][0]["properties"]["Transparency"] = serde_json::json!(0.1);
        meta["children"][1]["children"][0]["properties"]["Transparency"] = serde_json::json!(0.2);
        job.vfs
            .write(
                "/project/Props/init.meta.json",
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();
        let mut patch = transparency_update(base, 0.75);
        patch
            .updated_instances
            .extend(transparency_update(deep, 0.8).updated_instances);
        let cursor = job.message_queue.cursor();

        let (accepted, conflicts, errors) = studio_write(&job, patch);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 2);
        let meta = read_props_meta(&job);
        assert_eq!(
            meta_entry(&meta, &["Base"]).unwrap()["properties"]["Transparency"],
            0.75
        );
        assert_eq!(
            meta_entry(&meta, &["Nested", "Deep"]).unwrap()["properties"]["Transparency"],
            0.8
        );
        let disk_values = [Variant::Float32(0.1), Variant::Float32(0.2)];
        for message in messages_since(&job, cursor) {
            for update in message.updated {
                if let Some(Some(value)) = update.changed_properties.get(&ustr("Transparency")) {
                    assert!(
                        !disk_values.iter().any(|disk| variant_eq(disk, value)),
                        "Studio was sent a disk value it overwrites: {value:?}"
                    );
                }
            }
        }
        assert_disk_matches_server(&job, owner);
    }

    /// Renaming an inline parent and editing its child in one batch saves both,
    /// since the rename is applied before the child is compared with the disk.
    #[test]
    fn inline_renames_are_seen_by_later_edits_in_the_batch() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let (job, owner) = inline_job();
            let nested = find(&job, &["Nested"]);
            let deep = find(&job, &["Nested", "Deep"]);
            let mut patch = rename_update(nested, "Renamed");
            patch
                .updated_instances
                .extend(transparency_update(deep, 0.8).updated_instances);
            let (accepted, conflicts, errors) =
                job.handle_tree_event_at_cursor(patch, Some(job.message_queue.cursor()), authority);
            assert!(errors.is_empty(), "{authority:?}: {errors:?}");
            assert_eq!(conflicts, PatchSet::new(), "{authority:?}");
            assert_eq!(accepted.updated_instances.len(), 2, "{authority:?}");
            let meta = read_props_meta(&job);
            assert_eq!(
                meta_entry(&meta, &["Renamed", "Deep"]).unwrap()["properties"]["Transparency"],
                0.8,
                "{authority:?}"
            );
            assert_eq!(find(&job, &["Renamed", "Deep"]), deep, "{authority:?}");
            assert_disk_matches_server(&job, owner);
        }
    }

    /// An edit to an instance whose own file was deleted on disk fails under
    /// Studio authority, since two-way sync can't recreate it, and is only a
    /// conflict under filesystem authority.
    #[test]
    fn edits_to_deleted_sources_fail_only_under_studio_authority() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let (job, id) = part_job();
            job.vfs.remove_dir_all("/project/Part").unwrap();
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                transparency_update(id, 0.75),
                Some(job.message_queue.cursor()),
                authority,
            );
            assert!(accepted.updated_instances.is_empty(), "{authority:?}");
            assert_eq!(conflicts.removed_instances, vec![id], "{authority:?}");
            assert_eq!(
                errors.len(),
                (authority == WriteAuthority::Studio) as usize,
                "{authority:?}"
            );
        }
    }

    /// The same Studio edit meets a pending disk change under each authority.
    /// The filesystem restores the disk value. Studio writes its edit and keeps
    /// the disk's unrelated rename and property change.
    #[test]
    fn studio_authority_overwrites_only_what_studio_edited() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let (job, id) = part_job();
            job.vfs
                .write(
                    "/project/Part/init.meta.json",
                    r#"{"className":"Part","name":"DiskPart","properties":{"Transparency":0.5,"CanCollide":false,"Reflectance":0.3}}"#,
                )
                .unwrap();
            let cursor = job.message_queue.cursor();
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                transparency_update(id, 0.75),
                Some(cursor),
                authority,
            );
            assert!(errors.is_empty(), "{authority:?}: {errors:?}");
            let metadata: serde_json::Value = serde_json::from_str(
                &job.vfs
                    .read_to_string("/project/Part/init.meta.json")
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(metadata["name"], "DiskPart", "{authority:?}");
            assert_eq!(metadata["properties"]["Reflectance"], 0.3, "{authority:?}");

            let sent: Vec<_> = messages_since(&job, cursor)
                .into_iter()
                .flat_map(|message| message.updated)
                .collect();
            let tree = job.tree.lock().unwrap();
            let instance = tree.get_instance(id).unwrap();
            assert_eq!(instance.name(), "DiskPart", "{authority:?}");
            assert_eq!(
                instance.properties()[&ustr("Reflectance")],
                Variant::Float32(0.3),
                "{authority:?}"
            );
            match authority {
                WriteAuthority::Filesystem => {
                    assert!(accepted.updated_instances.is_empty());
                    assert_eq!(conflicts.updated_instances.len(), 1);
                    assert_eq!(metadata["properties"]["Transparency"], 0.5);
                }
                WriteAuthority::Studio => {
                    assert_eq!(accepted.updated_instances.len(), 1);
                    assert_eq!(conflicts, PatchSet::new());
                    assert_eq!(metadata["properties"]["Transparency"], 0.75);
                    assert_eq!(
                        instance.properties()[&ustr("Transparency")],
                        Variant::Float32(0.75)
                    );
                    // Studio is told about the rename and Reflectance, but never
                    // about the disk Transparency it just replaced.
                    assert!(sent
                        .iter()
                        .any(|update| update.changed_name.as_deref() == Some("DiskPart")));
                    assert!(sent.iter().any(|update| update
                        .changed_properties
                        .contains_key(&ustr("Reflectance"))));
                    assert!(sent.iter().all(|update| update
                        .changed_properties
                        .get(&ustr("Transparency"))
                        != Some(&Some(Variant::Float32(0.5)))));
                }
            }
        }
    }

    /// Studio writes retry a bounded number of times before giving up, and
    /// filesystem writes never retry.
    #[test]
    fn writes_that_race_a_disk_change_retry_only_under_studio_authority() {
        for attempt in 1..=STUDIO_WRITE_ATTEMPTS + 1 {
            assert_eq!(
                changed_during_write(WriteAuthority::Filesystem, attempt),
                ChangedDuringWrite::Restore
            );
            let expected = if attempt < STUDIO_WRITE_ATTEMPTS {
                ChangedDuringWrite::Retry
            } else {
                ChangedDuringWrite::GiveUp
            };
            assert_eq!(
                changed_during_write(WriteAuthority::Studio, attempt),
                expected,
                "attempt {attempt}"
            );
        }
    }

    /// A batch that started before the server's latest revision is only
    /// rejected for that reason when the filesystem is authoritative.
    #[test]
    fn studio_authority_accepts_stale_batches() {
        for (authority, saved) in [
            (WriteAuthority::Filesystem, false),
            (WriteAuthority::Studio, true),
        ] {
            let (job, id) = part_job();
            let (accepted, _, errors) =
                job.handle_tree_event_at_cursor(transparency_update(id, 0.75), None, authority);
            assert!(errors.is_empty(), "{authority:?}");
            assert_eq!(
                accepted.updated_instances.len(),
                saved as usize,
                "{authority:?}"
            );
            assert_eq!(
                job.vfs
                    .read_to_string("/project/Part/init.meta.json")
                    .unwrap()
                    .contains("0.75"),
                saved,
                "{authority:?}"
            );
        }
    }

    /// Studio deletions win over pending disk edits, but never remove files
    /// the server doesn't know about, since Studio never had them.
    #[test]
    fn studio_authority_deletions_still_keep_unknown_files() {
        for unknown_file in [false, true] {
            let (job, id) = part_job();
            job.vfs
                .write(
                    "/project/Part/init.meta.json",
                    r#"{"className":"Part","properties":{"Transparency":0.5}}"#,
                )
                .unwrap();
            if unknown_file {
                job.vfs.write("/project/Part/notes.txt", "keep me").unwrap();
            }
            let (accepted, conflicts, errors) = studio_write(
                &job,
                PatchSet {
                    removed_instances: vec![id],
                    ..PatchSet::new()
                },
            );
            // The folder comes back with an error that says why.
            assert_eq!(errors.len(), unknown_file as usize, "{errors:?}");
            if unknown_file {
                assert!(errors[0].message.contains("notes.txt"), "{errors:?}");
            }
            assert_eq!(accepted.removed_instances.is_empty(), unknown_file);
            assert_eq!(conflicts.added_instances.len(), unknown_file as usize);
            assert_eq!(
                job.vfs
                    .metadata("/project/Part")
                    .with_not_found()
                    .unwrap()
                    .is_some(),
                unknown_file
            );
        }
    }

    /// A project with one model file, whose root has an ID only its metadata
    /// holds.
    fn model_job() -> (JobThreadContext, Ref, Ref) {
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([(
                    "Extra.model.json",
                    VfsSnapshot::file(
                        r#"{"className":"Folder","id":"extra-id","children":[{"className":"Part","name":"Child"}]}"#,
                    ),
                )]),
            )],
            "/project/src",
        );
        let extra = find(&job, &["Extra"]);
        let child = find(&job, &["Extra", "Child"]);
        (job, extra, child)
    }

    fn rename(id: Ref, name: &str) -> PatchSet {
        PatchSet {
            updated_instances: vec![PatchUpdate {
                changed_name: Some(name.into()),
                ..empty_update(id)
            }],
            ..PatchSet::new()
        }
    }

    #[test]
    fn model_root_renames_rewrite_models_and_preserve_adjacent_metadata() {
        for extension in ["rbxm", "rbxmx"] {
            for sidecar in [false, true] {
                let dom = WeakDom::new(
                    InstanceBuilder::new("Folder").with_child(
                        InstanceBuilder::new("Folder")
                            .with_name("Original")
                            .with_child(InstanceBuilder::new("Part").with_name("Child")),
                    ),
                );
                let mut bytes = Vec::new();
                match extension {
                    "rbxm" => {
                        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap()
                    }
                    "rbxmx" => rbx_xml::to_writer(
                        &mut bytes,
                        &dom,
                        dom.root().children(),
                        rbx_xml::EncodeOptions::new()
                            .property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown),
                    )
                    .unwrap(),
                    _ => unreachable!(),
                }
                let filename = format!("Extra.{extension}");
                let path = format!("/project/src/{filename}");
                let mut files = vec![(filename.as_str(), VfsSnapshot::file(bytes))];
                if sidecar {
                    files.push((
                        "Extra.meta.json",
                        VfsSnapshot::file(r#"{"attributes":{"Keep":true}}"#),
                    ));
                }
                let (job, _) = script_job(vec![("src", VfsSnapshot::dir(files))], "/project/src");
                let extra = find(&job, &["Extra"]);
                let child = find(&job, &["Extra", "Child"]);
                let (accepted, conflicts, errors) = studio_write(&job, rename(extra, "Renamed"));
                assert!(errors.is_empty(), "{extension} {errors:?}");
                assert_eq!(conflicts, PatchSet::new());
                assert_eq!(accepted.updated_instances.len(), 1);
                assert!(exists(&job, &path));
                assert!(!exists(&job, "/project/src/Renamed"));
                let metadata: serde_json::Value = serde_json::from_slice(
                    &job.vfs
                        .read("/project/src/Extra.meta.json")
                        .unwrap_or_else(|error| panic!("{extension} sidecar={sidecar}: {error}")),
                )
                .unwrap();
                assert_eq!(metadata["name"], "Renamed");
                let snapshot =
                    snapshot_from_vfs(&Default::default(), &job.vfs, Path::new(&path)).unwrap();
                let tree = job.tree.lock().unwrap();
                assert_eq!(compute_patch_set(snapshot, &tree, extra), PatchSet::new());
                assert_eq!(tree.get_instance(child).unwrap().parent(), extra);
                assert_eq!(tree.get_instance(extra).unwrap().name(), "Renamed");
                if sidecar {
                    let Variant::Attributes(attributes) =
                        &tree.get_instance(extra).unwrap().properties()[&ustr("Attributes")]
                    else {
                        panic!("The model root lost its adjacent metadata attributes");
                    };
                    assert_eq!(attributes.get("Keep"), Some(&Variant::Bool(true)));
                }
                drop(tree);
                let previous_model = job.vfs.read(&path).unwrap();
                let previous_metadata = job.vfs.read("/project/src/Extra.meta.json").unwrap();
                let cursor = job.message_queue.cursor();
                job.message_queue.push_messages(&[AppliedPatchSet::new()]);
                let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                    rename(extra, "Rejected"),
                    Some(cursor),
                    WriteAuthority::Filesystem,
                );
                assert!(errors.is_empty(), "{errors:?}");
                assert!(accepted.updated_instances.is_empty());
                assert_eq!(conflicts.updated_instances.len(), 1);
                assert_eq!(job.vfs.read(&path).unwrap(), previous_model);
                assert_eq!(
                    job.vfs.read("/project/src/Extra.meta.json").unwrap(),
                    previous_metadata
                );
            }
        }
    }

    #[test]
    fn terrain_model_root_rename_preserves_voxels_clouds_and_tracks_lfs() {
        let smooth_grid = Variant::BinaryString(vec![0, 255, 128, 0, 17].into());
        let dom = WeakDom::new(
            InstanceBuilder::new("Folder").with_child(
                InstanceBuilder::new("Terrain")
                    .with_property("SmoothGrid", smooth_grid.clone())
                    .with_child(InstanceBuilder::new("Clouds").with_property("Cover", 0.75_f32)),
            ),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([("Terrain.rbxm", VfsSnapshot::file(bytes))]),
            )],
            "/project/src",
        );
        let terrain = find(&job, &["Terrain"]);
        let (accepted, conflicts, errors) = studio_write(&job, rename(terrain, "Landscape"));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 1);
        let bytes = job.vfs.read("/project/src/Terrain.rbxm").unwrap();
        let saved = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let root = saved.get_by_ref(saved.root().children()[0]).unwrap();
        assert_eq!(root.name, "Landscape");
        assert_eq!(root.class, "Terrain");
        assert_eq!(root.properties[&ustr("SmoothGrid")], smooth_grid);
        assert_eq!(root.children().len(), 1);
        let clouds = saved.get_by_ref(root.children()[0]).unwrap();
        assert_eq!(clouds.class, "Clouds");
        assert_eq!(clouds.properties[&ustr("Cover")], Variant::Float32(0.75));
        let metadata: serde_json::Value =
            serde_json::from_slice(&job.vfs.read("/project/src/Terrain.meta.json").unwrap())
                .unwrap();
        assert_eq!(metadata["name"], "Landscape");
        let attributes = job.vfs.read_to_string("/project/.gitattributes").unwrap();
        assert!(
            attributes.contains("\"/src/Terrain.rbxm\" filter=lfs diff=lfs merge=lfs -text"),
            "{attributes}"
        );
        let snapshot = snapshot_from_vfs(
            &Default::default(),
            &job.vfs,
            Path::new("/project/src/Terrain.rbxm"),
        )
        .unwrap();
        let tree = job.tree.lock().unwrap();
        assert_eq!(compute_patch_set(snapshot, &tree, terrain), PatchSet::new());
    }

    fn read_model(job: &JobThreadContext) -> String {
        String::from_utf8(
            job.vfs
                .read("/project/src/Extra.model.json")
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    /// A stale write was made without seeing a disk change, so under
    /// Filesystem authority it's refused, even once the watcher has brought
    /// the tree in line with the disk.
    #[test]
    fn stale_filesystem_model_edits_are_refused() {
        for stale in [false, true] {
            let (job, _, child) = model_job();
            let cursor = job.message_queue.cursor();
            let original = read_model(&job);
            if stale {
                job.message_queue.push_messages(&[AppliedPatchSet::new()]);
            }
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                rename(child, "Renamed"),
                Some(cursor),
                WriteAuthority::Filesystem,
            );
            assert!(errors.is_empty(), "{errors:?}");
            let model = read_model(&job);
            assert_eq!(model.contains("Renamed"), !stale, "{model}");
            assert_eq!(accepted.updated_instances.is_empty(), stale);
            assert_eq!(conflicts.updated_instances.is_empty(), !stale);
            if stale {
                assert_eq!(model, original);
                assert!(!exists(&job, "/project/src/Extra.meta.json"));
            }
        }
    }

    /// Deleting inside a model file follows the same rule as other writes. A
    /// disk change Studio hasn't seen wins under Filesystem authority, and is
    /// kept under Studio authority while the deletion is saved.
    #[test]
    fn model_deletions_catch_up_with_the_disk_first() {
        for authority in [WriteAuthority::Filesystem, WriteAuthority::Studio] {
            let (job, _, child) = model_job();
            job.vfs
                .write(
                    "/project/src/Extra.model.json",
                    r#"{"className":"Folder","id":"extra-id","attributes":{"Disk":true},"children":[{"className":"Part","name":"Child"}]}"#,
                )
                .unwrap();
            let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(
                PatchSet {
                    removed_instances: vec![child],
                    ..PatchSet::new()
                },
                Some(job.message_queue.cursor()),
                authority,
            );
            assert!(errors.is_empty(), "{errors:?}");
            let model = read_model(&job);
            assert!(model.contains("Disk"), "{model}");
            let deleted = authority == WriteAuthority::Studio;
            assert_eq!(model.contains("Child"), !deleted, "{model}");
            assert_eq!(accepted.removed_instances.contains(&child), deleted);
            assert_eq!(conflicts.added_instances.is_empty(), deleted);
        }
    }

    /// A model edit refuses the properties other two-way edits refuse, gives
    /// Studio their saved values back, and still saves the rest.
    #[test]
    fn model_edits_refuse_properties_that_dont_serialize() {
        let (job, _, child) = model_job();
        let mut update = rename(child, "Renamed");
        update.updated_instances[0].changed_properties.insert(
            ustr("Position"),
            Some(Variant::Vector3(Vector3::new(1.0, 2.0, 3.0))),
        );
        let (accepted, conflicts, errors) = studio_write(&job, update);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].property.as_deref(), Some("Position"));
        assert_eq!(accepted.updated_instances.len(), 1);
        assert!(conflicts.updated_instances[0]
            .changed_properties
            .contains_key(&ustr("Position")));
        let model = read_model(&job);
        assert!(
            model.contains("Renamed") && !model.contains("Position"),
            "{model}"
        );
    }

    /// Rewriting a model file keeps the ID its root's metadata held, so
    /// references to it still resolve.
    #[test]
    fn model_rewrites_keep_their_root_id() {
        let (job, extra, child) = model_job();
        let (_, _, errors) = studio_write(&job, rename(child, "Renamed"));
        assert!(errors.is_empty(), "{errors:?}");
        let model = read_model(&job);
        assert!(model.contains("extra-id"), "{model}");
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            tree.get_specified_id(&crate::RojoRef::new("extra-id".into())),
            Some(extra)
        );
    }

    /// A move out of a model file into a script that changed on disk is
    /// refused, and the moved instance is back in the tree under its ID, as
    /// the model file still holds it.
    #[test]
    fn refused_moves_under_scripts_put_the_instance_back() {
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([
                    (
                        "Extra.model.json",
                        VfsSnapshot::file(
                            r#"{"className":"Folder","children":[{"className":"Part","name":"Moved"}]}"#,
                        ),
                    ),
                    ("Code.luau", VfsSnapshot::file("return 1")),
                ]),
            )],
            "/project/src",
        );
        let moved = find(&job, &["Extra", "Moved"]);
        let extra = find(&job, &["Extra"]);
        let code = find(&job, &["Code"]);
        job.vfs.write("/project/src/Code.luau", "return 2").unwrap();

        // A property edit in the same update is still saved where it is.
        let mut update = parent_update(moved, code);
        update.updated_instances[0]
            .changed_properties
            .insert(ustr("Transparency"), Some(Variant::Float32(0.5)));
        let (_, _, errors) = job.handle_tree_event_at_cursor(
            update,
            Some(job.message_queue.cursor()),
            WriteAuthority::Filesystem,
        );

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("script"), "{errors:?}");
        let tree = job.tree.lock().unwrap();
        assert_eq!(tree.get_instance(moved).unwrap().parent(), extra);
        let model: serde_json::Value =
            serde_json::from_slice(&job.vfs.read("/project/src/Extra.model.json").unwrap())
                .unwrap();
        assert_eq!(model["children"][0]["name"], "Moved");
        assert_eq!(model["children"][0]["properties"]["Transparency"], 0.5);
        assert!(job
            .vfs
            .metadata("/project/src/Moved")
            .with_not_found()
            .unwrap()
            .is_none());
    }

    /// Deleting a file-backed instance takes its adjacent metadata with it, in
    /// either format, so nothing stale is left for a later file at that name.
    #[test]
    fn deletions_remove_adjacent_metadata() {
        for sidecar in ["Code.meta.json", "Code.meta.jsonc"] {
            let (job, id) = script_job(
                vec![
                    ("Code.luau", VfsSnapshot::file("return 1")),
                    (
                        sidecar,
                        VfsSnapshot::file(r#"{"attributes":{"Kind":"code"}}"#),
                    ),
                ],
                "/project/Code.luau",
            );
            let (accepted, _, errors) = studio_write(
                &job,
                PatchSet {
                    removed_instances: vec![id],
                    ..PatchSet::new()
                },
            );
            assert!(errors.is_empty(), "{sidecar} {errors:?}");
            assert_eq!(accepted.removed_instances, vec![id]);
            for path in ["/project/Code.luau", &format!("/project/{sidecar}")] {
                assert!(
                    job.vfs.metadata(path).with_not_found().unwrap().is_none(),
                    "{path} was left behind"
                );
            }
        }
    }

    /// Files that share a stem share their adjacent metadata, so deleting one
    /// of them leaves it for the other.
    #[test]
    fn deletions_keep_metadata_another_file_reads() {
        let (job, src) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([
                    ("Code.luau", VfsSnapshot::file("return 1")),
                    ("Code.txt", VfsSnapshot::file("text")),
                    (
                        "Code.meta.json",
                        VfsSnapshot::file(r#"{"attributes":{"Kind":"shared"}}"#),
                    ),
                ]),
            )],
            "/project/src",
        );
        let script = {
            let tree = job.tree.lock().unwrap();
            let children = tree.get_instance(src).unwrap().children().to_vec();
            children
                .into_iter()
                .find(|child| tree.get_instance(*child).unwrap().class_name() == "ModuleScript")
                .unwrap()
        };
        let (accepted, _, errors) = studio_write(
            &job,
            PatchSet {
                removed_instances: vec![script],
                ..PatchSet::new()
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(accepted.removed_instances, vec![script]);
        assert!(job
            .vfs
            .metadata("/project/src/Code.luau")
            .with_not_found()
            .unwrap()
            .is_none());
        for kept in ["/project/src/Code.txt", "/project/src/Code.meta.json"] {
            assert!(
                job.vfs.metadata(kept).with_not_found().unwrap().is_some(),
                "{kept}"
            );
        }
    }

    fn exists(job: &JobThreadContext, path: &str) -> bool {
        job.vfs.metadata(path).with_not_found().unwrap().is_some()
    }

    /// A `.gitkeep` is only needed while its directory is empty. Anything new
    /// below it, however deep, makes every empty one on the way up go, and a
    /// `.gitkeep` with contents or outside the project is left alone. Walks
    /// every combination of how deep the new file is and what the outer
    /// `.gitkeep` holds.
    #[test]
    fn new_contents_remove_the_gitkeeps_above_them() {
        for deep in [false, true] {
            for outer in ["", "notes"] {
                let (job, _) = script_job(
                    vec![(
                        "src",
                        VfsSnapshot::dir([(
                            "Outer",
                            VfsSnapshot::dir([
                                (".gitkeep", VfsSnapshot::file(outer)),
                                (
                                    "Inner",
                                    VfsSnapshot::dir([(".gitkeep", VfsSnapshot::file(""))]),
                                ),
                            ]),
                        )]),
                    )],
                    "/project/src",
                );
                job.vfs
                    .write("/outside.gitkeep-holder/.gitkeep", "")
                    .unwrap();
                let created = if deep {
                    "/project/src/Outer/Inner/New.luau"
                } else {
                    "/project/src/Outer/New.luau"
                };
                job.vfs.write(created, "return 1").unwrap();
                job.handle_vfs_event(VfsEvent::Create(created.into()));

                let case = format!("deep {deep} outer {outer:?}");
                assert_eq!(
                    exists(&job, "/project/src/Outer/Inner/.gitkeep"),
                    !deep,
                    "{case}"
                );
                assert_eq!(
                    exists(&job, "/project/src/Outer/.gitkeep"),
                    !outer.is_empty(),
                    "{case}"
                );
                assert!(exists(&job, "/outside.gitkeep-holder/.gitkeep"), "{case}");
                // The new file is still read as usual.
                let path: &[&str] = if deep {
                    &["Outer", "Inner", "New"]
                } else {
                    &["Outer", "New"]
                };
                find(&job, path);
            }
        }
    }

    /// Something new in a folder is read on its own, whether it's a file, a
    /// folder that already has contents, or only reported through a file deep
    /// inside it. Files Rojo doesn't read add nothing, and an `init` file still
    /// changes what its folder is.
    #[test]
    fn new_entries_are_added_without_rereading_their_folder() {
        let (job, _) = script_job(
            vec![(
                "src",
                VfsSnapshot::dir([(
                    "Holder",
                    VfsSnapshot::dir([("Existing.luau", VfsSnapshot::file("return 0"))]),
                )]),
            )],
            "/project/src",
        );
        let existing = find(&job, &["Holder", "Existing"]);
        job.vfs
            .write("/project/src/Holder/Existing.luau", "return 'unseen'")
            .unwrap();

        job.vfs
            .write("/project/src/Holder/File.luau", "return 1")
            .unwrap();
        job.handle_vfs_event(VfsEvent::Create("/project/src/Holder/File.luau".into()));
        job.vfs
            .create_dir_all("/project/src/Holder/Deep/Inner")
            .unwrap();
        job.vfs
            .write("/project/src/Holder/Deep/Inner/Leaf.luau", "return 2")
            .unwrap();
        job.handle_vfs_event(VfsEvent::Create(
            "/project/src/Holder/Deep/Inner/Leaf.luau".into(),
        ));
        job.vfs
            .write("/project/src/Holder/.DS_Store", "junk")
            .unwrap();
        job.handle_vfs_event(VfsEvent::Create("/project/src/Holder/.DS_Store".into()));

        find(&job, &["Holder", "File"]);
        find(&job, &["Holder", "Deep", "Inner", "Leaf"]);
        let holder = find(&job, &["Holder"]);
        {
            let tree = job.tree.lock().unwrap();
            // Only the new entries were read, so the unannounced edit wasn't.
            assert_eq!(
                tree.get_instance(existing)
                    .unwrap()
                    .properties()
                    .get(&ustr("Source")),
                Some(&Variant::String("return 0".into()))
            );
            let holder = tree.get_instance(holder).unwrap();
            assert_eq!(holder.children().len(), 3);
            assert_eq!(holder.class_name(), "Folder");
        }

        job.vfs
            .write("/project/src/Holder/init.luau", "return 'module'")
            .unwrap();
        job.handle_vfs_event(VfsEvent::Create("/project/src/Holder/init.luau".into()));
        let holder = find(&job, &["Holder"]);
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            tree.get_instance(holder).unwrap().class_name(),
            "ModuleScript"
        );
    }

    /// Deleting the last child of a folder from Studio keeps the folder in
    /// version control, while deleting one of several doesn't.
    #[test]
    fn deleting_the_last_child_keeps_its_folder() {
        for siblings in [false, true] {
            let mut entries = vec![("Only.luau", VfsSnapshot::file("return 1"))];
            if siblings {
                entries.push(("Other.luau", VfsSnapshot::file("return 2")));
            }
            let (job, _) = script_job(
                vec![(
                    "src",
                    VfsSnapshot::dir([("Holder", VfsSnapshot::dir(entries))]),
                )],
                "/project/src",
            );
            let only = find(&job, &["Holder", "Only"]);
            let (accepted, _, errors) = studio_write(
                &job,
                PatchSet {
                    removed_instances: vec![only],
                    ..PatchSet::new()
                },
            );
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(accepted.removed_instances, vec![only]);
            assert_eq!(exists(&job, "/project/src/Holder/.gitkeep"), !siblings);
        }
    }

    /// Syncback keeps empty directories with a `.gitkeep`, which no instance
    /// lists, so it must not stop the directory from being deleted.
    #[test]
    fn deletions_remove_the_gitkeep_syncback_wrote() {
        // An empty one is syncback's. One with contents is someone else's.
        for contents in ["", "notes"] {
            let (job, id) = part_job();
            job.vfs.write("/project/Part/.gitkeep", contents).unwrap();
            let (accepted, _, errors) = studio_write(
                &job,
                PatchSet {
                    removed_instances: vec![id],
                    ..PatchSet::new()
                },
            );
            let ours = contents.is_empty();
            assert_eq!(errors.is_empty(), ours, "{errors:?}");
            assert_eq!(accepted.removed_instances.contains(&id), ours);
            assert_eq!(
                job.vfs
                    .metadata("/project/Part")
                    .with_not_found()
                    .unwrap()
                    .is_none(),
                ours
            );
        }
    }

    /// An inline edit rewrites its owner's whole file, so disk changes to the
    /// other instances stored there must survive a Studio-authoritative write.
    #[test]
    fn studio_authority_inline_writes_keep_other_disk_changes() {
        let (job, owner) = inline_job();
        let base = find(&job, &["Base"]);
        let mut meta = read_props_meta(&job);
        meta["children"][0]["properties"]["Transparency"] = serde_json::json!(0.5);
        meta["children"][3]["properties"] = serde_json::json!({"Transparency": 0.9});
        job.vfs
            .write(
                "/project/Props/init.meta.json",
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();

        let (accepted, conflicts, errors) = studio_write(&job, transparency_update(base, 0.75));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 1);
        let meta = read_props_meta(&job);
        assert_eq!(
            meta_entry(&meta, &["Base"]).unwrap()["properties"]["Transparency"],
            0.75
        );
        assert_eq!(
            meta_entry(&meta, &["Sibling"]).unwrap()["properties"]["Transparency"],
            0.9
        );
        assert_disk_matches_server(&job, owner);
    }

    /// Two-way sync can't create instances, so an edit to one the disk has
    /// since removed fails, and Studio is sent the removal.
    #[test]
    fn studio_authority_reports_instances_removed_on_disk() {
        let (job, _) = inline_job();
        let deep = find(&job, &["Nested", "Deep"]);
        let mut meta = read_props_meta(&job);
        meta["children"][1]["children"] = serde_json::json!([]);
        job.vfs
            .write(
                "/project/Props/init.meta.json",
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();
        let cursor = job.message_queue.cursor();

        let (accepted, _, errors) = studio_write(&job, transparency_update(deep, 0.75));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(job.tree.lock().unwrap().get_instance(deep).is_none());
        assert!(messages_since(&job, cursor)
            .iter()
            .any(|message| message.removed.contains(&deep)));
    }

    fn find(job: &JobThreadContext, path: &[&str]) -> Ref {
        let tree = job.tree.lock().unwrap();
        let mut current = tree.root().children()[0];
        for name in path {
            current = *tree
                .get_instance(current)
                .unwrap()
                .children()
                .iter()
                .find(|child| tree.get_instance(**child).unwrap().name() == *name)
                .unwrap_or_else(|| panic!("{name} should exist"));
        }
        current
    }

    fn read_props_meta(job: &JobThreadContext) -> serde_json::Value {
        let contents = job
            .vfs
            .read_to_string("/project/Props/init.meta.json")
            .unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    /// Finds an inline entry in metadata by the names along its path.
    fn meta_entry<'a>(meta: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
        let mut current = meta;
        for name in path {
            current = current["children"]
                .as_array()?
                .iter()
                .find(|child| child["name"] == *name)?;
        }
        Some(current)
    }

    /// Asserts that the server tree matches what the disk now holds, which is
    /// what keeps the next watcher event from changing anything in Studio.
    fn assert_disk_matches_server(job: &JobThreadContext, owner: Ref) {
        let snapshot =
            snapshot_from_vfs(&Default::default(), &job.vfs, Path::new("/project/Props")).unwrap();
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            compute_patch_set(snapshot, &tree, owner),
            PatchSet::new(),
            "{}",
            job.vfs
                .read_to_string("/project/Props/init.meta.json")
                .unwrap()
        );
    }

    #[test]
    fn inline_owner_only_applies_to_directory_metadata_children() {
        let (job, owner) = inline_job();
        for (path, expected) in [
            (&[][..], None),
            (&["Base"][..], Some(owner)),
            (&["Nested"][..], Some(owner)),
            (&["Nested", "Deep"][..], Some(owner)),
            (&["InlineModule"][..], Some(owner)),
            // Model files keep their own files and are not stored in metadata.
            (&["Extra"][..], None),
            (&["Extra", "ModelChild"][..], None),
        ] {
            let id = find(&job, path);
            let tree = job.tree.lock().unwrap();
            assert_eq!(inline_owner(&tree, id), expected, "{path:?}");
        }
    }

    #[test]
    fn inline_edits_rewrite_only_the_owner_metadata() {
        #[derive(Debug, Clone, Copy)]
        enum Edit {
            Property,
            Attribute,
            Rename,
            Delete,
        }
        for path in [&[][..], &["Base"][..], &["Nested", "Deep"][..]] {
            for edit in [Edit::Property, Edit::Attribute, Edit::Rename, Edit::Delete] {
                if path.is_empty() && matches!(edit, Edit::Delete | Edit::Rename) {
                    // Relocating or deleting the owner is structural, covered separately.
                    continue;
                }
                let (job, owner) = inline_job();
                let id = find(&job, path);
                let untouched = [
                    find(&job, &["Sibling"]),
                    find(&job, &["Pointer"]),
                    find(&job, &["Extra", "ModelChild"]),
                ];
                let extra = job.vfs.read("/project/Props/Extra.model.json").unwrap();
                let patch = match edit {
                    Edit::Property => transparency_update(id, 0.75),
                    Edit::Attribute => {
                        // Studio sends every attribute, including Rojo's own.
                        let tree = job.tree.lock().unwrap();
                        let mut attributes = match tree
                            .get_instance(id)
                            .unwrap()
                            .properties()
                            .get(&ustr("Attributes"))
                        {
                            Some(Variant::Attributes(attributes)) => attributes.clone(),
                            _ => Attributes::new(),
                        };
                        drop(tree);
                        attributes.insert("Count".into(), Variant::Float64(2.0));
                        PatchSet {
                            updated_instances: vec![PatchUpdate {
                                id,
                                changed_name: None,
                                changed_class_name: None,
                                changed_parent: None,
                                changed_properties: UstrMap::from_iter([(
                                    ustr("Attributes"),
                                    Some(Variant::Attributes(attributes)),
                                )]),
                                changed_metadata: None,
                            }],
                            ..PatchSet::new()
                        }
                    }
                    Edit::Rename => rename_update(id, "Renamed"),
                    Edit::Delete => PatchSet {
                        removed_instances: vec![id],
                        ..PatchSet::new()
                    },
                };
                let (accepted, conflicts, errors) = job.handle_tree_event(patch);
                let case = format!("{path:?} {edit:?}");
                assert!(errors.is_empty(), "{case}: {errors:?}");
                assert_eq!(conflicts, PatchSet::new(), "{case}");
                match edit {
                    Edit::Delete => assert_eq!(accepted.removed_instances, vec![id], "{case}"),
                    _ => assert_eq!(accepted.updated_instances.len(), 1, "{case}"),
                }

                let meta = read_props_meta(&job);
                let mut edited_path = path.to_vec();
                if let (Edit::Rename, Some(last)) = (edit, edited_path.last_mut()) {
                    *last = "Renamed";
                }
                let entry = meta_entry(&meta, &edited_path);
                match edit {
                    Edit::Property => {
                        assert_eq!(entry.unwrap()["properties"]["Transparency"], 0.75)
                    }
                    Edit::Attribute => {
                        assert_eq!(entry.unwrap()["attributes"]["Count"], serde_json::json!(2))
                    }
                    Edit::Rename if path.is_empty() => assert_eq!(meta["name"], "Renamed"),
                    Edit::Rename => assert!(entry.is_some(), "{case}: {meta:#}"),
                    Edit::Delete => assert!(entry.is_none(), "{case}: {meta:#}"),
                }
                // Untouched entries keep their IDs and references on disk.
                if !(path == ["Base"] && matches!(edit, Edit::Rename | Edit::Delete)) {
                    assert_eq!(
                        meta_entry(&meta, &["Base"]).unwrap()["attributes"]["Rojo_Id"],
                        "base-id",
                        "{case}"
                    );
                }
                assert_eq!(
                    meta_entry(&meta, &["Sibling"]).unwrap()["id"],
                    "sibling-id",
                    "{case}"
                );
                assert_eq!(
                    meta_entry(&meta, &["Pointer"]).unwrap()["attributes"]["Rojo_Target_Value"],
                    "base-id",
                    "{case}"
                );
                assert!(meta_entry(&meta, &["Extra"]).is_none(), "{case}");
                assert_eq!(
                    job.vfs.read("/project/Props/Extra.model.json").unwrap(),
                    extra,
                    "{case}"
                );

                if path == ["Base"] && matches!(edit, Edit::Delete) {
                    // The pointer's target is gone, so the disk now reads it as nil.
                    // The watcher's next event sends exactly that to Studio.
                    let snapshot = snapshot_from_vfs(
                        &Default::default(),
                        &job.vfs,
                        Path::new("/project/Props"),
                    )
                    .unwrap();
                    let tree = job.tree.lock().unwrap();
                    let residual = compute_patch_set(snapshot, &tree, owner);
                    assert_eq!(residual.updated_instances.len(), 1, "{case}");
                    assert_eq!(residual.updated_instances[0].id, untouched[1], "{case}");
                    assert_eq!(
                        residual.updated_instances[0].changed_properties,
                        UstrMap::from_iter([(ustr("Value"), None)]),
                        "{case}"
                    );
                } else {
                    assert_disk_matches_server(&job, owner);
                }
                let tree = job.tree.lock().unwrap();
                for untouched in untouched {
                    assert!(tree.get_instance(untouched).is_some(), "{case}");
                }
            }
        }
    }

    #[test]
    fn inline_script_source_edits_are_written_to_metadata() {
        let (job, owner) = inline_job();
        let id = find(&job, &["InlineModule"]);
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    ustr("Source"),
                    Some(Variant::String("return 2".into())),
                )]),
                changed_metadata: None,
            }],
            ..PatchSet::new()
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(accepted.updated_instances.len(), 1);
        assert_eq!(
            read_props_meta(&job)["children"][4]["properties"]["Source"],
            "return 2"
        );
        assert_disk_matches_server(&job, owner);
    }

    #[test]
    fn inline_writes_merge_properties_but_protect_deletions_and_unknown_baselines() {
        // Ordinary fields merge unrelated sibling edits. Deletions and requests
        // without a valid baseline retain filesystem-authoritative restoration.
        for (stale, disk_edit) in [(true, false), (false, true), (true, true)] {
            for removal in [false, true] {
                let (job, owner) = inline_job();
                let base = find(&job, &["Base"]);
                let sibling = find(&job, &["Sibling"]);
                if disk_edit {
                    job.vfs
                        .write(
                            "/project/Props/init.meta.json",
                            INLINE_META.replace(
                                r#""name": "Sibling", "id""#,
                                r#""name": "Sibling", "properties": { "Transparency": 0.9 }, "id""#,
                            ),
                        )
                        .unwrap();
                }
                let before = job.vfs.read("/project/Props/init.meta.json").unwrap();
                let patch = if removal {
                    PatchSet {
                        removed_instances: vec![base],
                        ..PatchSet::new()
                    }
                } else {
                    transparency_update(base, 0.75)
                };
                let cursor = (!stale).then(|| job.message_queue.cursor());
                let (accepted, conflicts, errors) =
                    job.handle_tree_event_at_cursor(patch, cursor, WriteAuthority::Filesystem);
                let case = format!("stale={stale} disk_edit={disk_edit} removal={removal}");
                if !removal && !stale {
                    assert_eq!(accepted.updated_instances.len(), 1, "{case}");
                    assert!(errors.is_empty(), "{case} {errors:?}");
                    assert!(
                        conflicts.updated_instances.is_empty(),
                        "{case} {conflicts:?}"
                    );
                    let tree = job.tree.lock().unwrap();
                    assert_eq!(
                        tree.get_instance(base).unwrap().properties()[&ustr("Transparency")],
                        Variant::Float32(0.75)
                    );
                    assert_eq!(
                        tree.get_instance(sibling).unwrap().properties()[&ustr("Transparency")],
                        Variant::Float32(0.9)
                    );
                    drop(tree);
                    assert_disk_matches_server(&job, owner);
                    continue;
                }
                assert_eq!(accepted, PatchSet::new(), "{case}");
                assert!(errors.is_empty(), "{case}: {errors:?}");
                assert_eq!(
                    job.vfs.read("/project/Props/init.meta.json").unwrap(),
                    before,
                    "{case}"
                );
                if removal {
                    assert_eq!(conflicts.added_instances.len(), 1, "{case}");
                } else {
                    let restored = conflicts
                        .updated_instances
                        .iter()
                        .find(|update| update.id == base)
                        .unwrap();
                    assert_eq!(
                        restored.changed_properties[&ustr("Transparency")],
                        Some(Variant::Float32(0.25)),
                        "{case}"
                    );
                }
                assert_disk_matches_server(&job, owner);
                let tree = job.tree.lock().unwrap();
                // Restoration keeps the server's instances rather than replacing them.
                assert!(tree.get_instance(base).is_some(), "{case}");
                assert!(tree.get_instance(sibling).is_some(), "{case}");
                assert_eq!(
                    tree.get_instance(owner)
                        .unwrap()
                        .children()
                        .iter()
                        .filter(|child| tree.get_instance(**child).unwrap().name() == "Base")
                        .count(),
                    1,
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn disk_edits_patch_inline_instances_without_replacing_untouched_ones() {
        let (job, owner) = inline_job();
        let before: Vec<_> = ["Base", "Nested", "Pointer", "Sibling", "InlineModule"]
            .iter()
            .map(|name| find(&job, &[name]))
            .collect();
        let deep = find(&job, &["Nested", "Deep"]);
        job.vfs
            .write(
                "/project/Props/init.meta.json",
                INLINE_META
                    .replace(r#""Transparency": 0.5"#, r#""Transparency": 0.625"#)
                    .replace(
                        r#""name": "Sibling", "id": "sibling-id" }"#,
                        r#""name": "Sibling", "id": "sibling-id" },
                        { "className": "Part", "name": "Added" }"#,
                    ),
            )
            .unwrap();
        job.handle_vfs_event(VfsEvent::Write(PathBuf::from(
            "/project/Props/init.meta.json",
        )));
        assert_eq!(
            ["Base", "Nested", "Pointer", "Sibling", "InlineModule"]
                .iter()
                .map(|name| find(&job, &[name]))
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(find(&job, &["Nested", "Deep"]), deep);
        find(&job, &["Added"]);
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            tree.get_instance(deep).unwrap().properties()[&ustr("Transparency")],
            Variant::Float32(0.625)
        );
        // Five inline children, one added, and the model file child.
        assert_eq!(tree.get_instance(owner).unwrap().children().len(), 7);
        drop(tree);
        assert_disk_matches_server(&job, owner);
    }

    #[test]
    fn inline_references_resolve_to_their_targets() {
        let (job, _) = inline_job();
        let base = find(&job, &["Base"]);
        let pointer = find(&job, &["Pointer"]);
        let tree = job.tree.lock().unwrap();
        assert_eq!(
            tree.get_instance(pointer).unwrap().properties()[&ustr("Value")],
            Variant::Ref(base)
        );
        assert_eq!(
            tree.get_specified_id(&crate::RojoRef::new("base-id".into())),
            Some(base)
        );
    }

    fn transparency_update(id: Ref, value: f32) -> PatchSet {
        PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    "Transparency".into(),
                    Some(Variant::Float32(value)),
                )]),
                changed_metadata: None,
            }],
        }
    }

    fn rename_update(id: Ref, name: &str) -> PatchSet {
        PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: Some(name.to_owned()),
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            }],
        }
    }

    #[test]
    fn storage_context_conflict_restores_disk_then_allows_later_writes() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/project", VfsSnapshot::empty_dir())
            .unwrap();
        imfs.load_snapshot(
            "/project/default.project.json",
            VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
        )
        .unwrap();
        imfs.load_snapshot("/project/Part", VfsSnapshot::empty_dir())
            .unwrap();
        imfs.load_snapshot(
            "/project/Part/init.meta.json",
            VfsSnapshot::file(
                r#"{
                    "className": "Part",
                    "id": "fixture-id",
                    "properties": { "Transparency": 0.3 },
                    "attributes": { "FixtureAttribute": "initial" }
                }"#,
            ),
        )
        .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let mut attributes = Attributes::new();
        attributes.insert(
            "FixtureAttribute".to_string(),
            Variant::String("initial".into()),
        );
        let root = InstanceSnapshot::new()
            .name("Part")
            .class_name("Part")
            .properties(UstrMap::from_iter([
                ("Transparency".into(), Variant::Float32(0.5)),
                ("Attributes".into(), Variant::Attributes(attributes)),
            ]))
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(PathBuf::from("/project/Part"))
                    .relevant_paths(vec![
                        PathBuf::from("/project/Part/init.meta.json"),
                        PathBuf::from("/project/Part/init.meta.jsonc"),
                    ])
                    .specified_id(Some(crate::RojoRef::new("fixture-id".into())))
                    .middleware(crate::snapshot_middleware::Middleware::Dir),
            );
        let tree = Arc::new(Mutex::new(RojoTree::new(
            InstanceSnapshot::new().children(vec![root]),
        )));
        let mut job = JobThreadContext {
            tree: Arc::clone(&tree),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };
        let id = tree.lock().unwrap().root().children()[0];
        // The handcrafted baseline lacks the directory middleware's full source/watch
        // metadata, so this discrepancy is structural, not an ordinary property conflict.
        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.7));
        assert!(accepted.updated_instances.is_empty());
        assert!(errors.is_empty());
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            conflicts.updated_instances[0]
                .changed_properties
                .get(&ustr("Transparency")),
            Some(&Some(Variant::Float32(0.3)))
        );
        let contents = vfs.read_to_string("/project/Part/init.meta.json").unwrap();
        assert!(contents.contains("0.3"));
        vfs.write(
            "/project/Part/init.meta.json",
            r#"{
                "className": "Part",
                "id": "fixture-id",
                "properties": { "Transparency": 0.3, "CanCollide": false },
                "attributes": { "FixtureAttribute": "initial" }
            }"#,
        )
        .unwrap();
        job.handle_vfs_event(VfsEvent::Write(PathBuf::from(
            "/project/Part/init.meta.json",
        )));

        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.7));
        assert_eq!(accepted.updated_instances.len(), 1);
        assert!(conflicts.updated_instances.is_empty());
        assert!(errors.is_empty());
        let contents = vfs.read_to_string("/project/Part/init.meta.json").unwrap();
        assert!(contents.starts_with("{\n    \"className\": \"Part\""));
        let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(metadata["id"].as_str(), Some("fixture-id"));
        let transparency = metadata["properties"]["Transparency"].as_f64().unwrap();
        assert!((transparency - 0.7).abs() < 1e-6);
        assert_eq!(metadata["properties"]["CanCollide"].as_bool(), Some(false));
        assert!(contents.contains("FixtureAttribute"));

        let (accepted, conflicts, errors) = job.handle_tree_event(rename_update(id, "RenamedPart"));
        assert_eq!(accepted.updated_instances.len(), 1);
        assert_eq!(
            accepted.updated_instances[0].changed_name.as_deref(),
            Some("RenamedPart")
        );
        assert!(conflicts.updated_instances.is_empty());
        assert!(errors.is_empty());
        let moved_metadata = "/project/RenamedPart/init.meta.json";
        let contents = vfs.read_to_string(moved_metadata).unwrap();
        assert!(contents.starts_with("{\n    \"className\": \"Part\""));
        assert!(!contents.contains("\"name\""));

        vfs.write(
            moved_metadata,
            r#"{
                "className": "Part",
                "name": "DiskPart",
                "properties": { "Transparency": 0.7, "CanCollide": false },
                "attributes": { "FixtureAttribute": "initial" }
            }"#,
        )
        .unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(rename_update(id, "StudioPart"));
        assert!(accepted.updated_instances.is_empty());
        assert_eq!(conflicts.updated_instances.len(), 1);
        assert_eq!(
            conflicts.updated_instances[0].changed_name.as_deref(),
            Some("DiskPart")
        );
        assert!(errors.is_empty());
        assert!(tree.lock().unwrap().get_instance(id).unwrap().name() == "DiskPart");

        vfs.write(
            moved_metadata,
            r#"{
                "className": "Part",
                "name": "DiskPart",
                "properties": { "Transparency": 0.9, "CanCollide": false },
                "attributes": { "FixtureAttribute": "initial" }
            }"#,
        )
        .unwrap();
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: vec![id],
            added_instances: Vec::new(),
            updated_instances: Vec::new(),
        });
        assert!(accepted.removed_instances.is_empty());
        assert_eq!(conflicts.added_instances.len(), 1);
        assert!(errors.is_empty());
        assert!(vfs.read_to_string(moved_metadata).unwrap().contains("0.9"));

        let metadata = tree.lock().unwrap().get_metadata(id).unwrap().clone();
        tree.lock().unwrap().update_metadata(
            id,
            metadata
                .clone()
                .middleware(crate::snapshot_middleware::Middleware::Rbxm),
        );
        let (accepted, conflicts, errors) = job.handle_tree_event(transparency_update(id, 0.8));
        assert!(accepted.updated_instances.is_empty());
        assert!(!conflicts.updated_instances.is_empty());
        assert!(errors.is_empty());

        tree.lock().unwrap().update_metadata(
            id,
            metadata.middleware(crate::snapshot_middleware::Middleware::Dir),
        );
        Arc::get_mut(job.project.project.get_mut().unwrap())
            .unwrap()
            .syncback_rules = Some(
            serde_json::from_value(serde_json::json!({
                "ignoreProperties": { "Part": ["CanCollide"] }
            }))
            .unwrap(),
        );
        tree.lock()
            .unwrap()
            .get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .insert(ustr("CanCollide"), Variant::Bool(false));
        let (accepted, _, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    "CanCollide".into(),
                    Some(Variant::Bool(true)),
                )]),
                changed_metadata: None,
            }],
        });
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        let contents = vfs.read_to_string(moved_metadata).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&contents).unwrap()["properties"]
                ["CanCollide"]
                .as_bool(),
            Some(false)
        );
    }

    #[test]
    fn metadata_only_script_edit_preserves_source_text() {
        let source = "return { message = \"keep\\r\\n\" }\r\n";
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project/default.project.json",
            VfsSnapshot::file(r#"{"name":"Test","tree":{"$className":"Folder"}}"#),
        )
        .unwrap();
        imfs.load_snapshot("/project/Script.luau", VfsSnapshot::file(source))
            .unwrap();
        let vfs = Arc::new(Vfs::new(imfs));
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(
            &vfs,
            &PathBuf::from("/project/default.project.json"),
            Some("Test"),
        )
        .unwrap();
        let root = snapshot_from_vfs(
            &Default::default(),
            &vfs,
            &PathBuf::from("/project/Script.luau"),
        )
        .unwrap()
        .unwrap();
        let tree = Arc::new(Mutex::new(RojoTree::new(root)));
        let job = JobThreadContext {
            tree: Arc::clone(&tree),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };
        let id = tree.lock().unwrap().get_root_id();
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_properties: UstrMap::from_iter([(
                    "RunContext".into(),
                    Some(Variant::String("Server".into())),
                )]),
                changed_metadata: None,
            }],
        });
        assert_eq!(accepted.updated_instances.len(), 1);
        assert!(conflicts.updated_instances.is_empty());
        assert!(errors.is_empty());
        assert_eq!(
            vfs.read("/project/Script.luau").unwrap().as_slice(),
            source.as_bytes()
        );
        assert!(vfs
            .read_to_string("/project/Script.meta.json")
            .unwrap()
            .contains("Server"));
    }

    #[test]
    fn deleted_paths_and_directories_do_not_crash_event_processing() {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project_path = temp.path().join("default.project.json");
        std::fs::write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
        let job = JobThreadContext {
            tree: Arc::new(Mutex::new(RojoTree::new(InstanceSnapshot::new()))),
            vfs: Arc::new(vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project).into(),
        };
        let file = temp.path().join("deleted.luau");
        std::fs::write(&file, "return nil").unwrap();
        std::fs::remove_file(&file).unwrap();
        job.handle_vfs_event(VfsEvent::Write(file));
        let directory = temp.path().join("Deleted");
        std::fs::create_dir(&directory).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        job.handle_vfs_event(VfsEvent::Remove(directory));
    }
}
