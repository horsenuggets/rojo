use crossbeam_channel::{select, Receiver, RecvError, Sender};
use jod_thread::JoinHandle;
use memofs::{IoResultExt, Vfs, VfsEvent};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, HashMapExt as _, InstanceBuilder, UstrMap, WeakDom,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{
    message_queue::MessageQueue,
    project::Project,
    snapshot::{
        apply_patch_set, compute_patch_set, compute_shallow_patch_set, AppliedPatchSet,
        InstanceSnapshot, InstigatingSource, PatchAdd, PatchSet, PatchUpdate, RojoTree,
    },
    snapshot_middleware::{
        snapshot_from_vfs, snapshot_project_node, snapshot_shallow_from_vfs, Middleware,
    },
    syncback::{
        can_write_property, decide_property_write, get_property_filter, is_json_compatible,
        source_file, syncback_instance, FsSnapshot, PropertyWriteDecision,
    },
    web::interface::{WriteError, WriteResult},
};

/// A mutation and the channel dedicated to its own result.
pub(crate) struct TreeMutation {
    pub message_cursor: Option<u32>,
    pub patch_set: PatchSet,
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
        message_queue: Arc<MessageQueue<AppliedPatchSet>>,
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
            project,
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
                            let result = task.handle_tree_event_at_cursor(
                                mutation.patch_set,
                                mutation.message_cursor,
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
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,

    /// The project's serialization and filtering rules.
    project: Arc<Project>,
}

impl JobThreadContext {
    /// Computes and applies patches to the DOM for a given file path.
    ///
    /// This function finds the nearest ancestor to the given path that has associated instances
    /// in the tree.
    /// It then computes and applies changes for each affected instance ID and
    /// returns a vector of applied patch sets.
    fn apply_patches(&self, path: PathBuf) -> Vec<AppliedPatchSet> {
        let mut tree = self.tree.lock().unwrap();
        let mut applied_patches = Vec::new();

        // Find the nearest ancestor to this path that has
        // associated instances in the tree. This helps make sure
        // that we handle additions correctly, especially if we
        // receive events for descendants of a large tree being
        // created all at once.
        let mut current_path = path.as_path();
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

        for id in affected_ids {
            // A change to one of an instance's own files, like a directory's
            // init.meta.json or init.luau, can only change that instance and
            // what its metadata stores. Re-reading the directory's whole subtree
            // for it would make every edit to a large folder take minutes.
            let own_file = current_path == path.as_path()
                && tree
                    .get_metadata(id)
                    .and_then(|metadata| metadata.instigating_source.as_ref())
                    .is_some_and(|source| source.path() != current_path);
            if let Some(patch) = compute_and_apply_changes(&mut tree, &self.vfs, id, own_file) {
                if !patch.is_empty() {
                    applied_patches.push(patch);
                }
            }
        }

        applied_patches
    }

    fn handle_vfs_event(&self, event: VfsEvent) {
        log::trace!("Vfs event: {:?}", event);

        // Update the VFS immediately with the event.
        self.vfs
            .commit_event(&event)
            .expect("Error applying VFS change");

        // For a given VFS event, we might have many changes to different parts
        // of the tree. Calculate and apply all of these changes.
        let applied_patches = match event {
            VfsEvent::Create(path) | VfsEvent::Write(path) => {
                self.apply_patches(canonicalize_or_original(&self.vfs, path))
            }
            VfsEvent::Remove(path) => match (path.parent(), path.file_name()) {
                (Some(parent), Some(file_name)) => {
                    let parent_normalized =
                        canonicalize_or_original(&self.vfs, parent.to_path_buf());
                    self.apply_patches(parent_normalized.join(file_name))
                }
                _ => {
                    log::warn!("Cannot identify the removed path");
                    Vec::new()
                }
            },
            _ => {
                log::warn!("Unhandled VFS event: {:?}", event);
                Vec::new()
            }
        };

        // Notify anyone listening to the message queue about the changes we
        // just made.
        self.message_queue.push_messages(&applied_patches);
    }

    #[cfg(test)]
    fn handle_tree_event(&self, patch_set: PatchSet) -> WriteResult {
        self.handle_tree_event_at_cursor(patch_set, Some(self.message_queue.cursor()))
    }

    fn handle_tree_event_at_cursor(
        &self,
        patch_set: PatchSet,
        message_cursor: Option<u32>,
    ) -> WriteResult {
        // A request is based on the last revision applied before its local batch began.
        // The current server DOM is not a baseline when a watcher has advanced it.
        let stale = message_cursor != Some(self.message_queue.cursor());
        log::trace!("Persisting PatchSet from client: {:#?}", patch_set);
        let mut accepted = PatchSet::new();
        let mut conflicts = PatchSet::new();
        let mut errors = Vec::new();
        let mut messages = Vec::new();
        let mut deletion_conflicts = Vec::new();
        {
            let mut tree = self.tree.lock().unwrap();
            for &id in &patch_set.removed_instances {
                if let Some(owner) = inline_owner(&tree, id) {
                    let latest = match self.remove_inline_instance(&tree, id, owner, stale) {
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
                match self.remove_instance(&tree, id, stale) {
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
                    Ok(None) => accepted.removed_instances.push(id),
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
            for update in &patch_set.updated_instances {
                self.persist_update(
                    &mut tree,
                    update,
                    &mut accepted,
                    &mut conflicts,
                    &mut errors,
                    stale,
                );
            }
        }
        if accepted != PatchSet::new() {
            let mut tree = self.tree.lock().unwrap();
            messages.push(apply_patch_set(&mut tree, accepted.clone()));
        }
        if conflicts != PatchSet::new() {
            let mut tree = self.tree.lock().unwrap();
            messages.push(apply_patch_set(&mut tree, conflicts.clone()));
        }
        conflicts.added_instances.extend(deletion_conflicts);
        self.message_queue.push_messages(&messages);
        (accepted, conflicts, errors)
    }

    fn remove_instance(
        &self,
        tree: &RojoTree,
        id: Ref,
        stale: bool,
    ) -> anyhow::Result<Option<crate::snapshot::InstanceSnapshot>> {
        let instance = tree
            .get_instance(id)
            .ok_or_else(|| anyhow::anyhow!("Cannot remove unknown instance {id:?}"))?;
        let Some(InstigatingSource::Path(path)) = instance.metadata().instigating_source.as_ref()
        else {
            anyhow::bail!("Cannot remove an instance without a unique filesystem source");
        };
        let snapshot = snapshot_from_vfs(&instance.metadata().context, &self.vfs, path)?;
        let disk_patch = compute_patch_set(snapshot.clone(), tree, id);
        if stale || disk_patch != PatchSet::new() {
            return Ok(snapshot);
        }
        // A snapshot excludes ignored and unrecognized files. Never delete those as
        // a side effect of removing a represented directory from Studio.
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
                    if !tracked.contains(entry.path()) {
                        return Ok(snapshot);
                    }
                    if self.vfs.metadata(entry.path())?.is_dir() {
                        pending.push(entry.path().to_path_buf());
                    }
                }
            }
        }
        let mut lock = self.vfs.lock();
        match lock.metadata(path).with_not_found()? {
            Some(metadata) if metadata.is_file() => lock.remove_file(path)?,
            Some(_) => lock.remove_dir_all(path)?,
            None => {}
        }
        Ok(None)
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
        tree: &RojoTree,
        id: Ref,
        owner: Ref,
        stale: bool,
    ) -> anyhow::Result<InlineRemoval> {
        let owner_instance = tree
            .get_instance(owner)
            .ok_or_else(|| anyhow::anyhow!("The owning directory is no longer known"))?;
        let expected = FsSnapshot::capture_file_contents(
            &owner_instance.metadata().relevant_paths,
            &self.vfs,
        )?;
        let snapshot = self.snapshot_owner(tree, owner)?;
        if stale || compute_shallow_patch_set(snapshot.clone(), tree, owner) != PatchSet::new() {
            return Ok(InlineRemoval::Restore(Box::new(snapshot)));
        }
        let new_tree = inline_subtree(tree, owner, &InlineEdit::Remove(id));
        let staged = syncback_instance(&self.vfs, tree, &self.project, owner, new_tree)?;
        let latest = self.snapshot_owner(tree, owner)?;
        if snapshots_differ(snapshot, latest.as_ref()) {
            return Ok(InlineRemoval::Restore(Box::new(latest)));
        }
        staged.write_single_file_to_vfs(self.project.folder_location(), &self.vfs, &expected)?;
        Ok(InlineRemoval::Removed)
    }

    fn persist_update(
        &self,
        tree: &mut RojoTree,
        update: &PatchUpdate,
        accepted: &mut PatchSet,
        conflicts: &mut PatchSet,
        errors: &mut Vec<WriteError>,
        stale: bool,
    ) {
        let id = update.id;
        let Some(instance) = tree.get_instance(id) else {
            errors.push(WriteError {
                id,
                property: None,
                message: "Cannot update an unknown instance".into(),
            });
            return;
        };
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
            return;
        }
        if stale || compute_shallow_patch_set(snapshot.clone(), tree, owner_id) != PatchSet::new() {
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
        // With no discrepancy on disk, an inline instance's values on the server
        // are exactly what its owner's file stores.
        let disk_root = snapshot.as_ref().unwrap();
        let (disk_name, disk_properties) = match inline_owner {
            Some(_) => (instance.name(), instance.properties()),
            None => (disk_root.name.as_ref() as &str, &disk_root.properties),
        };
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
        for (key, studio_value) in &update.changed_properties {
            let baseline_value = instance.properties().get(key);
            let disk_value = disk_properties.get(key);
            match decide_property_write(baseline_value, disk_value, studio_value.as_ref()) {
                PropertyWriteDecision::AlreadySynchronized => {}
                PropertyWriteDecision::RestoreFilesystem => {
                    conflict_properties.insert(*key, disk_value.cloned());
                }
                PropertyWriteDecision::Persist => {
                    if !can_write_property(
                        &self.project,
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
                    } else if studio_value
                        .as_ref()
                        .is_some_and(|value| !is_json_compatible(value))
                    {
                        errors.push(WriteError {
                            id,
                            property: Some(key.to_string()),
                            message: "This value cannot be represented in metadata".into(),
                        });
                    } else {
                        accepted_properties.insert(*key, studio_value.clone());
                    }
                }
            }
        }
        let filter_tree = WeakDom::new(InstanceBuilder::new(instance.class_name()));
        if let Some(ignored) = get_property_filter(&self.project, filter_tree.root()) {
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
            let disk_value = disk_properties.get(key);
            if !accepted_properties.contains_key(key) && disk_value != studio_value.as_ref() {
                conflict_properties.insert(*key, disk_value.cloned());
            }
        }
        if !conflict_properties.is_empty() || conflict_name.is_some() {
            conflicts.updated_instances.push(PatchUpdate {
                id,
                changed_name: conflict_name,
                changed_class_name: None,
                changed_properties: conflict_properties,
                changed_metadata: None,
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
        let mut staged = match syncback_instance(&self.vfs, tree, &self.project, owner_id, new_tree)
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Failed to serialize changes: {err:#}"),
                });
                conflicts.updated_instances.retain(|patch| patch.id != id);
                conflicts
                    .updated_instances
                    .extend(disk_restoration(tree, owner_id, update, snapshot).updated_instances);
                return;
            }
        };
        let source = source_file(&owner);
        if !accepted_properties.contains_key(&ustr("Source")) {
            // A directory script's source lives in its initializer, not at the
            // directory path, so both are left out when Source wasn't edited.
            staged.retain_added_files(|candidate| {
                candidate != path && Some(candidate) != source.as_deref()
            });
        } else if let Some(source) = source
            .as_deref()
            .filter(|_| inline_owner.is_none() && accepted_name.is_none())
            .filter(|_| accepted_properties.len() == 1)
        {
            // Only Source changed, which lives in the source file alone. Leaving
            // metadata out keeps its formatting and the write to one file.
            staged.retain_added_files(|candidate| candidate == source);
        }
        // Every accepted value differs from the disk, so a write that changes no
        // file means the edit can't be represented and must not report Saved.
        if !staged.changes_files(self.project.folder_location(), &expected) {
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
        let latest = match snapshot_shallow_from_vfs(&owner.metadata().context, &self.vfs, path) {
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
        if let Some(disk_patch) = revalidation_conflicts(tree, owner_id, update, snapshot, latest) {
            conflicts.updated_instances.retain(|patch| patch.id != id);
            conflicts
                .removed_instances
                .extend(disk_patch.removed_instances);
            conflicts.added_instances.extend(disk_patch.added_instances);
            conflicts
                .updated_instances
                .extend(disk_patch.updated_instances);
            return;
        }
        match staged.write_single_file_to_vfs(self.project.folder_location(), &self.vfs, &expected)
        {
            Ok(()) => accepted.updated_instances.push(PatchUpdate {
                id,
                changed_name: accepted_name,
                changed_class_name: None,
                changed_properties: accepted_properties,
                changed_metadata: None,
            }),
            Err(err) => {
                errors.push(WriteError {
                    id,
                    property: None,
                    message: format!("Failed to persist changes: {err:#}"),
                });
                match snapshot_shallow_from_vfs(&owner.metadata().context, &self.vfs, path) {
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
    }
}

fn revalidation_conflicts(
    tree: &RojoTree,
    owner: Ref,
    update: &PatchUpdate,
    baseline: Option<InstanceSnapshot>,
    latest: Option<InstanceSnapshot>,
) -> Option<PatchSet> {
    if !snapshots_differ(baseline, latest.as_ref()) {
        return None;
    }

    Some(disk_restoration(tree, owner, update, latest))
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
        return filesystem_restoration(tree, update, latest);
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
) -> PatchSet {
    let mut patch = compute_shallow_patch_set(latest.clone(), tree, update.id);
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
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            });
        restoration.changed_properties.extend(
            update
                .changed_properties
                .keys()
                .map(|key| (*key, latest.properties.get(key).cloned())),
        );
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

fn canonicalize_or_original(vfs: &Vfs, path: PathBuf) -> PathBuf {
    match vfs.canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(err) => {
            log::warn!("Could not canonicalize {}: {err}", path.display());
            path
        }
    }
}

/// Re-reads the source of `id` and applies the difference. A `shallow` refresh
/// leaves out children that are stored in their own files, which have their own
/// change events.
fn compute_and_apply_changes(
    tree: &mut RojoTree,
    vfs: &Vfs,
    id: Ref,
    shallow: bool,
) -> Option<AppliedPatchSet> {
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
    let applied_patch_set = match instigating_source {
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

                let patch_set = if shallow {
                    compute_shallow_patch_set(snapshot, tree, id)
                } else {
                    compute_patch_set(snapshot, tree, id)
                };
                apply_patch_set(tree, patch_set)
            }
            Ok(None) => {
                // Our instance was previously created from a path, but that
                // path no longer exists.
                //
                // We associate deleting the instigating file for an
                // instance with deleting that instance.

                let mut patch_set = PatchSet::new();
                patch_set.removed_instances.push(id);

                apply_patch_set(tree, patch_set)
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

            let patch_set = compute_patch_set(snapshot, tree, id);
            apply_patch_set(tree, patch_set)
        }
    };

    Some(applied_patch_set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::InstanceMetadata;
    use memofs::{InMemoryFs, VfsSnapshot};
    use rbx_dom_weak::{types::Attributes, ustr};
    use std::path::Path;

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
                project: Arc::new(project),
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
                project: Arc::new(project),
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
                changed_properties: UstrMap::from_iter([(ustr(key), Some(value))]),
                changed_metadata: None,
            }],
        }
    }

    fn read_text(vfs: &Vfs, path: &str) -> Option<String> {
        vfs.read(path)
            .with_not_found()
            .unwrap()
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
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
            project: Arc::new(project),
        };

        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id: zone_id,
                changed_name: None,
                changed_class_name: None,
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
            project: Arc::new(project),
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
            project: Arc::new(project),
        };

        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id: root_id,
                changed_name: Some("Renamed".into()),
                changed_class_name: None,
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            }],
        });

        assert_eq!(accepted, PatchSet::new());
        assert_eq!(conflicts, PatchSet::new());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Project-defined"));
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
        let patch = revalidation_conflicts(&tree, id, &update, baseline.clone(), latest).unwrap();
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
        let patch = revalidation_conflicts(&tree, id, &update, baseline, None).unwrap();
        assert_eq!(patch.removed_instances, vec![id]);
    }

    #[test]
    fn stale_missing_and_future_cursors_restore_disk_without_writing() {
        for cursor in [None, Some(0), Some(u32::MAX)] {
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
            let (accepted, conflicts, errors) =
                job.handle_tree_event_at_cursor(transparency_update(id, 0.75), cursor);
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
        Arc::get_mut(&mut job.project).unwrap().syncback_rules =
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
        Arc::get_mut(&mut job.project).unwrap().syncback_rules =
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
    fn pending_disk_discrepancy_restores_before_attempting_serialization() {
        let (mut job, id) = part_job();
        Arc::get_mut(&mut job.project).unwrap().syncback_rules =
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
        assert!(errors.is_empty());
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
        Arc::get_mut(&mut job.project).unwrap().syncback_rules =
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
                project: Arc::new(project),
            },
            id,
        )
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
                if path.is_empty() && matches!(edit, Edit::Delete) {
                    // Deleting the owner removes the directory, covered elsewhere.
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
                    Edit::Attribute => assert_eq!(
                        entry.unwrap()["attributes"]["Count"],
                        serde_json::json!(2.0)
                    ),
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
    fn stale_or_discrepant_inline_writes_restore_the_disk() {
        // Each case gives the filesystem a reason to win: a stale cursor, or an
        // unprocessed disk change elsewhere in the same owner file.
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
                let (accepted, conflicts, errors) = job.handle_tree_event_at_cursor(patch, cursor);
                let case = format!("stale={stale} disk_edit={disk_edit} removal={removal}");
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
                changed_properties: UstrMap::new(),
                changed_metadata: None,
            }],
        }
    }

    #[test]
    fn conflicting_studio_write_restores_disk_then_allows_later_writes() {
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
        let tree = Arc::new(Mutex::new(RojoTree::new(root)));
        let mut job = JobThreadContext {
            tree: Arc::clone(&tree),
            vfs: Arc::clone(&vfs),
            message_queue: Arc::new(MessageQueue::new()),
            project: Arc::new(project),
        };
        let id = tree.lock().unwrap().get_root_id();
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
        let contents = vfs.read_to_string("/project/Part/init.meta.json").unwrap();
        assert!(contents.starts_with("{\n    \"className\": \"Part\""));
        assert!(contents.contains("\"name\": \"RenamedPart\""));

        vfs.write(
            "/project/Part/init.meta.json",
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
        assert!(tree.lock().unwrap().root().name() == "DiskPart");

        vfs.write(
            "/project/Part/init.meta.json",
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
        assert!(vfs
            .read_to_string("/project/Part/init.meta.json")
            .unwrap()
            .contains("0.9"));

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
        Arc::get_mut(&mut job.project).unwrap().syncback_rules = Some(
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
                changed_properties: UstrMap::from_iter([(
                    "CanCollide".into(),
                    Some(Variant::Bool(true)),
                )]),
                changed_metadata: None,
            }],
        });
        assert!(accepted.updated_instances.is_empty());
        assert!(!errors.is_empty());
        let contents = vfs.read_to_string("/project/Part/init.meta.json").unwrap();
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
            project: Arc::new(project),
        };
        let id = tree.lock().unwrap().get_root_id();
        let (accepted, conflicts, errors) = job.handle_tree_event(PatchSet {
            removed_instances: Vec::new(),
            added_instances: Vec::new(),
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
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
            project: Arc::new(project),
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
