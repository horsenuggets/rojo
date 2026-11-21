use std::{
    collections::HashSet,
    io,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use crossbeam_channel::Sender;
use memofs::Vfs;
use thiserror::Error;

use crate::{
    change_processor::{ChangeProcessor, TreeMutation},
    message_queue::MessageQueue,
    project::{Project, ProjectError},
    session_id::SessionId,
    snapshot::{
        apply_patch_set, compute_patch_set, InstanceContext, InstanceSnapshot, RequireContext,
        RojoTree,
    },
    snapshot_middleware::snapshot_from_vfs,
};

/// Contains all of the state for a Rojo serve session. A serve session is used
/// when we need to build a Rojo tree and possibly rebuild it when input files
/// change.
///
/// Nothing here is specific to any Rojo interface. Though the primary way to
/// interact with a serve session is Rojo's HTTP right now, there's no reason
/// why Rojo couldn't expose an IPC or channels-based API for embedding in the
/// future. `ServeSession` would be roughly the right interface to expose for
/// those cases.
pub struct ServeSession {
    /// The object responsible for listening to changes from the in-memory
    /// filesystem, applying them, updating the Roblox instance tree, and
    /// routing messages through the session's message queue to any connected
    /// clients.
    ///
    /// SHOULD BE DROPPED FIRST! ServeSession and ChangeProcessor communicate
    /// with eachother via channels. If ServeSession hangs up those channels
    /// before dropping the ChangeProcessor, its thread will panic with a
    /// RecvError, causing the main thread to panic on drop.
    ///
    /// Allowed to be unused because it has side effects when dropped.
    #[allow(unused)]
    change_processor: ChangeProcessor,

    /// When the serve session was started. Used only for user-facing
    /// diagnostics.
    start_time: Instant,

    /// The root project for the serve session.
    ///
    /// This will be defined if a folder with a `default.project.json` file was
    /// used for starting the serve session, or if the user specified a full
    /// path to a `.project.json` file.
    root_project: Arc<Project>,

    /// A randomly generated ID for this serve session. It's used to ensure that
    /// a client doesn't begin connecting to a different server part way through
    /// an operation that needs to be atomic.
    session_id: SessionId,

    /// The tree of Roblox instances associated with this session that will be
    /// updated in real-time. This is derived from the session's VFS and will
    /// eventually be mutable to connected clients.
    tree: Arc<Mutex<RojoTree>>,

    /// An in-memory filesystem containing all of the files relevant for this
    /// live session.
    ///
    /// The main use for accessing it from the session is for debugging issues
    /// with Rojo's live-sync protocol.
    vfs: Arc<Vfs>,

    /// A queue of changes that have been applied to `tree` that affect clients.
    ///
    /// Clients to the serve session will subscribe to this queue either
    /// directly or through the HTTP API to be notified of mutations that need
    /// to be applied.
    message_queue: Arc<MessageQueue>,

    /// A channel to send mutation requests on. These will be handled by the
    /// ChangeProcessor and trigger changes in the tree.
    tree_mutation_sender: Sender<TreeMutation>,

    export_jobs: Arc<crate::web::export::ExportJobs>,
}

// Validate relayout witnesses before either recovery pass mutates sources. Legacy
// records remain conservative; paths alone cannot prove historical package ownership.
fn preflight_startup_recovery(
    vfs: &Vfs,
    project: &Project,
    packages: Option<&crate::package_sources::PackageSources>,
) -> io::Result<()> {
    use memofs::IoResultExt;
    use std::path::Component;

    let root = project.folder_location();
    let failure = |path: &Path, reason: &str| {
        io::Error::other(format!(
            "Startup recovery refused for {} ({reason}). Journal and backups retained. Restore the original canonical directories and Wally ownership, or copy backups to a safe location and reconcile manually before retrying",
            path.display()
        ))
    };
    for cache in [
        crate::syncback::relocation::cache_directory(project),
        crate::syncback::relocation::relayout_directory(project),
    ] {
        for ancestor in cache.ancestors() {
            if let Some(canonical) = vfs.canonicalize(ancestor).with_not_found()? {
                if canonical != ancestor {
                    return Err(failure(
                        &cache,
                        "Recovery cache traverses a noncanonical ancestor",
                    ));
                }
                break;
            }
        }
    }
    let relayouts = crate::syncback::relocation::relayout_directory(project);
    let Some(folders) = vfs.read_dir(&relayouts).with_not_found()? else {
        return Ok(());
    };
    for folder in folders {
        let folder = folder?.path().to_path_buf();
        if vfs.canonicalize(&folder)? != folder {
            return Err(failure(
                &folder,
                "Relayout backup directory is not canonical",
            ));
        }
        let Some(bytes) = vfs
            .read(folder.join(crate::syncback::relocation::RELAYOUT_MANIFEST_NAME))
            .with_not_found()?
        else {
            continue;
        };
        let Ok(manifest) =
            serde_json::from_slice::<crate::syncback::relocation::RelayoutManifest>(&bytes)
        else {
            // The legacy recovery pass preserves unreadable manifests unchanged.
            continue;
        };
        manifest.validate_package(vfs, &folder)?;
        let has_package_evidence = manifest.package.is_some();
        for (original, backup) in manifest.entries {
            if !original.starts_with(root)
                || original
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
                || Path::new(&backup).components().count() != 1
                || !matches!(
                    Path::new(&backup).components().next(),
                    Some(Component::Normal(_))
                )
            {
                return Err(failure(
                    &folder,
                    "Legacy relayout contains an escaping or nonnormalized path",
                ));
            }
            let Some(parent) = original.parent() else {
                return Err(failure(&folder, "Relayout original has no parent"));
            };
            if vfs.canonicalize(parent).with_not_found()?.as_deref() != Some(parent) {
                return Err(failure(
                    &folder,
                    "Relayout original parent is missing or noncanonical",
                ));
            }
            if let Some(packages) = packages {
                if !has_package_evidence
                    && (packages.is_local_source(&original)
                        || packages.blocked(vfs, &original, true).is_some())
                {
                    return Err(failure(
                        &folder,
                        "Legacy relayout has no durable package ownership witness",
                    ));
                }
            }
        }
    }
    Ok(())
}

impl ServeSession {
    /// Start a new serve session from the given in-memory filesystem and start
    /// path.
    ///
    /// The project file is expected to be loaded out-of-band since it's
    /// currently loaded from the filesystem directly instead of through the
    /// in-memory filesystem layer.
    /// Start a new serve session from the given in-memory filesystem and
    /// start path.
    ///
    /// The project file is expected to be loaded out-of-band since it's
    /// currently loaded from the filesystem directly instead of through
    /// the in-memory filesystem layer.
    pub fn new<P: AsRef<Path>>(vfs: Vfs, start_path: P) -> Result<Self, ServeSessionError> {
        Self::new_with_pool(vfs, start_path, None)
    }

    pub(crate) fn new_with_pool<P: AsRef<Path>>(
        vfs: Vfs,
        start_path: P,
        pool: Option<Arc<rayon::ThreadPool>>,
    ) -> Result<Self, ServeSessionError> {
        let start_time = Instant::now();
        let start_path = vfs.canonicalize(start_path.as_ref())?;
        let start_path = start_path.as_path();

        log::trace!("Starting new ServeSession at path {}", start_path.display());

        let root_project = Arc::new(Project::load_initial_project(&vfs, start_path)?);
        // Establish package authority before recovery can touch original sources.
        // Replacement journals additionally verify their durable generation witness;
        // missing or stale provenance never grants rollback ownership.
        let package_sources =
            crate::package_sources::PackageSources::load(&vfs, root_project.folder_location())?;
        preflight_startup_recovery(&vfs, &root_project, package_sources.as_ref())?;
        crate::syncback::relocation::recover_relocations(&vfs, &root_project)?;
        crate::syncback::relocation::recover_relayouts(&vfs, &root_project)?;

        let mut tree = RojoTree::new(InstanceSnapshot::new());

        let root_id = tree.get_root_id();

        let mut instance_context =
            InstanceContext::with_emit_legacy_scripts(root_project.emit_legacy_scripts);

        // Journals are private server state, never source instances.
        instance_context.set_reserved_cache_path(root_project.folder_location().join(".rojo"));

        // Ordinary application recovery can restore manifests. Use the final
        // filesystem state rather than keeping a pre-recovery ownership snapshot.
        instance_context.package_sources =
            crate::package_sources::PackageSources::load(&vfs, root_project.folder_location())?
                .map(Arc::new);

        log::trace!("Generating snapshot of instances from VFS");
        let load = || snapshot_from_vfs(&instance_context, &vfs, start_path);
        let mut snapshot = match &pool {
            Some(pool) => pool.install(load),
            None => load(),
        }?;
        if let Some(snapshot) = &mut snapshot {
            // Use the expanded snapshot, not only explicit project nodes. This
            // includes nested default projects and referenced top-level projects.
            let require_context = build_require_context(snapshot, root_project.folder_location())?;
            crate::snapshot_middleware::prepare_requires(snapshot, &vfs, &require_context)?;
        }

        log::trace!("Computing initial patch set");
        let patch_set = compute_patch_set(snapshot, &tree, root_id);

        log::trace!("Applying initial patch set");
        apply_patch_set(&mut tree, patch_set);

        let session_id = SessionId::new();
        let message_queue = MessageQueue::new();

        let tree = Arc::new(Mutex::new(tree));
        let message_queue = Arc::new(message_queue);
        let vfs = Arc::new(vfs);

        let (tree_mutation_sender, tree_mutation_receiver) = crossbeam_channel::unbounded();

        log::trace!("Starting ChangeProcessor");
        let change_processor = ChangeProcessor::start(
            Arc::clone(&tree),
            Arc::clone(&vfs),
            Arc::clone(&message_queue),
            tree_mutation_receiver,
            pool,
            Arc::clone(&root_project),
        );

        Ok(Self {
            change_processor,
            start_time,
            session_id,
            root_project,
            tree,
            message_queue,
            tree_mutation_sender,
            vfs,
            export_jobs: Arc::default(),
        })
    }

    pub(crate) fn export_jobs(&self) -> Arc<crate::web::export::ExportJobs> {
        Arc::clone(&self.export_jobs)
    }

    pub fn tree_handle(&self) -> Arc<Mutex<RojoTree>> {
        Arc::clone(&self.tree)
    }

    pub fn tree(&self) -> MutexGuard<'_, RojoTree> {
        self.tree.lock().unwrap()
    }

    pub fn tree_mutation_sender(&self) -> Sender<TreeMutation> {
        self.tree_mutation_sender.clone()
    }

    #[allow(unused)]
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn message_queue(&self) -> &MessageQueue {
        &self.message_queue
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn project_name(&self) -> &str {
        self.root_project
            .name
            .as_ref()
            .expect("all top-level projects must have their name set")
    }

    pub fn project_port(&self) -> Option<u16> {
        self.root_project.serve_port
    }

    pub fn place_id(&self) -> Option<u64> {
        self.root_project.place_id
    }

    pub fn game_id(&self) -> Option<u64> {
        self.root_project.game_id
    }

    pub fn start_time(&self) -> Instant {
        self.start_time
    }

    pub fn serve_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.serve_place_ids.as_ref()
    }

    pub fn blocked_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.blocked_place_ids.as_ref()
    }

    pub fn serve_address(&self) -> Option<IpAddr> {
        self.root_project.serve_address
    }

    pub fn serve_allowed_hosts(&self) -> &[String] {
        &self.root_project.serve_allowed_hosts
    }

    pub fn root_dir(&self) -> &Path {
        self.root_project.folder_location()
    }

    pub fn root_project(&self) -> &Project {
        &self.root_project
    }
}

#[derive(Debug, Error)]
pub enum ServeSessionError {
    #[error(transparent)]
    Io {
        #[from]
        source: io::Error,
    },

    #[error(transparent)]
    Project {
        #[from]
        source: ProjectError,
    },

    #[error(transparent)]
    Other {
        #[from]
        source: anyhow::Error,
    },
}

#[profiling::function]
pub(crate) fn build_require_context(
    snapshot: &InstanceSnapshot,
    root: &Path,
) -> anyhow::Result<Arc<RequireContext>> {
    Ok(Arc::new(RequireContext {
        project_root: root.to_path_buf(),
        path_mappings: extract_path_mappings(snapshot, root)?,
        datamodel: snapshot.class_name == "DataModel",
    }))
}

#[profiling::function]
pub(crate) fn build_require_context_from_tree(
    tree: &RojoTree,
    root: &Path,
) -> anyhow::Result<Arc<RequireContext>> {
    fn walk(
        tree: &RojoTree,
        id: rbx_dom_weak::types::Ref,
        root: &Path,
        dm: &mut Vec<String>,
        mappings: &mut Vec<(PathBuf, Vec<String>)>,
    ) {
        let instance = tree.get_instance(id).unwrap();
        for path in instance.metadata().watch_paths() {
            if let Ok(relative) = path.strip_prefix(root) {
                mappings.push((relative.to_path_buf(), dm.clone()));
            }
        }
        for child in instance.children() {
            dm.push(tree.get_instance(*child).unwrap().name().to_owned());
            walk(tree, *child, root, dm, mappings);
            dm.pop();
        }
    }
    let mut mappings = Vec::new();
    {
        profiling::scope!("Rojo require mappings tree traversal");
        walk(
            tree,
            tree.get_root_id(),
            root,
            &mut Vec::new(),
            &mut mappings,
        );
    }
    Ok(Arc::new(RequireContext {
        project_root: root.to_path_buf(),
        path_mappings: normalize_path_mappings(mappings, root, &tree.root().metadata().context)?,
        datamodel: tree.root().class_name() == "DataModel",
    }))
}

/// Recursively extracts `$path` mappings from the project tree.
/// Each entry maps a filesystem path (relative to the project root)
/// to the DataModel path segments leading to that node.
///
/// For example, given:
/// ```json
/// { "Packages": { "$path": "Packages" },
///   "Plugin": { "$path": "Source/Plugin" } }
/// ```
/// This produces:
/// - `("Packages", ["Packages"])`
/// - `("Source/Plugin", ["Plugin"])`
fn extract_path_mappings(
    snapshot: &InstanceSnapshot,
    root: &Path,
) -> anyhow::Result<Vec<(PathBuf, Vec<String>)>> {
    let mut mappings = Vec::new();
    fn walk(
        snapshot: &InstanceSnapshot,
        root: &Path,
        dm: &mut Vec<String>,
        mappings: &mut Vec<(PathBuf, Vec<String>)>,
    ) {
        for path in snapshot.metadata.watch_paths() {
            if let Ok(relative) = path.strip_prefix(root) {
                mappings.push((relative.to_path_buf(), dm.clone()));
            }
        }
        for child in &snapshot.children {
            dm.push(child.name.to_string());
            walk(child, root, dm, mappings);
            dm.pop();
        }
    }
    {
        profiling::scope!("Rojo require mappings snapshot traversal");
        walk(snapshot, root, &mut Vec::new(), &mut mappings);
    }
    normalize_path_mappings(mappings, root, &snapshot.metadata.context)
}

fn normalize_path_mappings(
    mut mappings: Vec<(PathBuf, Vec<String>)>,
    root: &Path,
    context: &InstanceContext,
) -> anyhow::Result<Vec<(PathBuf, Vec<String>)>> {
    profiling::scope!("Rojo require mappings normalization");
    mappings.sort_by(|a, b| {
        b.0.components()
            .count()
            .cmp(&a.0.components().count())
            .then(a.0.cmp(&b.0))
    });
    mappings.dedup();
    if let Some(packages) = &context.package_sources {
        for pair in mappings.windows(2) {
            if pair[0].0 == pair[1].0 && packages.is_local_source(&root.join(&pair[0].0)) {
                anyhow::bail!("Local package source {} is mounted more than once. Remove the duplicate source mount instead of creating divergent module identities", pair[0].0.display());
            }
        }
    }
    Ok(mappings)
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::StdBackend;

    #[test]
    fn borrowed_require_context_matches_snapshot_mappings() {
        use crate::snapshot::InstanceMetadata;

        for class_name in ["DataModel", "Folder", "ModuleScript"] {
            let root = Path::new("/root");
            let mut util_metadata = InstanceMetadata::new().relevant_paths(vec![
                root.join("Source/Shared/Util.luau"),
                root.join("Source/Shared/Util.meta.json"),
                PathBuf::from("/outside/Util.luau"),
            ]);
            util_metadata.installed_paths = vec![
                root.join("Packages/Util.luau"),
                root.join("Packages/Util.luau"),
            ];
            let snapshot = InstanceSnapshot::new()
                .name("RootNameIsNotAMappingSegment")
                .class_name(class_name)
                .metadata(
                    InstanceMetadata::new().relevant_paths(vec![root.join("default.project.json")]),
                )
                .children(vec![
                    InstanceSnapshot::new()
                        .name("Shared")
                        .class_name("Folder")
                        .metadata(
                            InstanceMetadata::new()
                                .relevant_paths(vec![root.join("Source/Shared")]),
                        )
                        .children(vec![InstanceSnapshot::new()
                            .name("Util")
                            .class_name("ModuleScript")
                            .metadata(util_metadata)
                            .property("Source", "return 42")]),
                    InstanceSnapshot::new()
                        .name("Other")
                        .class_name("Folder")
                        .metadata(InstanceMetadata::new().relevant_paths(vec![root.join("Other")])),
                ]);
            let expected = build_require_context(&snapshot, root).unwrap();
            let tree = RojoTree::new(snapshot);
            let actual = build_require_context_from_tree(&tree, root).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(actual.datamodel, class_name == "DataModel");
            assert_eq!(actual.project_root, root);
            assert_eq!(
                actual.path_mappings,
                vec![
                    (
                        "Source/Shared/Util.luau".into(),
                        vec!["Shared".into(), "Util".into()]
                    ),
                    (
                        "Source/Shared/Util.meta.json".into(),
                        vec!["Shared".into(), "Util".into()]
                    ),
                    (
                        "Packages/Util.luau".into(),
                        vec!["Shared".into(), "Util".into()]
                    ),
                    ("Source/Shared".into(), vec!["Shared".into()]),
                    ("Other".into(), vec!["Other".into()]),
                    ("default.project.json".into(), vec![]),
                ]
            );
            assert_eq!(
                build_require_context_from_tree(&tree, root).unwrap(),
                actual
            );
        }
    }

    #[test]
    fn borrowed_require_context_rejects_duplicate_local_package_mappings() {
        use crate::{package_sources::PackageSources, snapshot::InstanceMetadata};
        use memofs::{InMemoryFs, VfsSnapshot};

        let root = Path::new("/root");
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot(
                root,
                VfsSnapshot::dir([
                    (
                        "wally.toml",
                        VfsSnapshot::file("[dependencies]\nLibrary = \"^Local/Library\"\n"),
                    ),
                    (
                        "Local",
                        VfsSnapshot::dir([(
                            "Library",
                            VfsSnapshot::dir([("wally.toml", VfsSnapshot::file(""))]),
                        )]),
                    ),
                ]),
            )
            .unwrap();
        let packages = PackageSources::load(&Vfs::new(backend), root)
            .unwrap()
            .unwrap();
        assert!(packages.is_local_source(&root.join("Local/Library")));
        let mut metadata = InstanceMetadata::new();
        metadata.context.package_sources = Some(Arc::new(packages));
        let snapshot = InstanceSnapshot::new()
            .class_name("Folder")
            .metadata(metadata)
            .children(
                ["First", "Second"]
                    .into_iter()
                    .map(|name| {
                        InstanceSnapshot::new()
                            .name(name)
                            .class_name("Folder")
                            .metadata(
                                InstanceMetadata::new()
                                    .relevant_paths(vec![root.join("Local/Library")]),
                            )
                    })
                    .collect::<Vec<_>>(),
            );
        let expected = build_require_context(&snapshot, root).unwrap_err();
        let tree = RojoTree::new(snapshot);
        let actual = build_require_context_from_tree(&tree, root).unwrap_err();
        assert_eq!(actual.to_string(), expected.to_string());
        assert!(actual.to_string().contains("mounted more than once"));
    }

    fn root_mapping_with_reserved_cache() -> memofs::InMemoryFs {
        use memofs::{InMemoryFs, VfsSnapshot};
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                (
                    "serve.project.json",
                    VfsSnapshot::file(
                        r#"{
                            "name": "CacheTest",
                            "globIgnorePaths": ["!*", "serve.project.json"],
                            "tree": { "$path": "." }
                        }"#,
                    ),
                ),
                (
                    ".rojo",
                    VfsSnapshot::dir([("intent.json", VfsSnapshot::file("invalid JSON"))]),
                ),
                ("Code.luau", VfsSnapshot::file("return 1")),
                (
                    ".rojo-backup",
                    VfsSnapshot::dir([("Backup.luau", VfsSnapshot::file("return {}"))]),
                ),
                (
                    "Source",
                    VfsSnapshot::dir([(
                        ".rojo",
                        VfsSnapshot::dir([("Nested.luau", VfsSnapshot::file("return {}"))]),
                    )]),
                ),
            ]),
        )
        .unwrap();
        imfs
    }

    #[test]
    fn root_mapping_cannot_reinclude_reserved_cache_with_ignore_negation() {
        let session = ServeSession::new(
            Vfs::new(root_mapping_with_reserved_cache()),
            "/root/serve.project.json",
        )
        .unwrap();
        let tree = session.tree();
        let mut children: Vec<_> = tree
            .root()
            .children()
            .iter()
            .map(|id| tree.get_instance(*id).unwrap().name())
            .collect();
        children.sort();
        assert_eq!(children, [".rojo-backup", "Code", "Source"]);
        for path in [
            "/root/Code.luau",
            "/root/.rojo-backup/Backup.luau",
            "/root/Source/.rojo/Nested.luau",
        ] {
            assert!(!tree.get_ids_at_path(Path::new(path)).is_empty(), "{path}");
        }
        assert!(tree.get_ids_at_path(Path::new("/root/.rojo")).is_empty());
        assert!(tree
            .get_metadata(tree.get_root_id())
            .unwrap()
            .context
            .is_reserved_path(Path::new("/root/.rojo/intent.json")));
    }

    #[test]
    fn watch_reload_keeps_reserved_cache_excluded_while_normal_source_changes() {
        use memofs::{VfsEvent, VfsSnapshot};
        let mut imfs = root_mapping_with_reserved_cache();
        let session =
            ServeSession::new(Vfs::new(imfs.clone()), "/root/serve.project.json").unwrap();
        let cursor = session.message_queue().cursor();
        imfs.load_snapshot(
            "/root/.rojo/another.json",
            VfsSnapshot::file("invalid JSON"),
        )
        .unwrap();
        imfs.raise_event(VfsEvent::Create("/root/.rojo/another.json".into()));
        // A root directory reload must skip the cache and still reconcile source changes.
        imfs.load_snapshot("/root/Code.luau", VfsSnapshot::file("return 42"))
            .unwrap();
        imfs.raise_event(VfsEvent::Write("/root/Code.luau".into()));
        imfs.raise_event(VfsEvent::Write("/root".into()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut cursor = cursor;
                loop {
                    let (next_cursor, _) = session.message_queue().subscribe(cursor).await.unwrap();
                    cursor = next_cursor;
                    let tree = session.tree();
                    assert!(tree.get_ids_at_path(Path::new("/root/.rojo")).is_empty());
                    let ids = tree.get_ids_at_path(Path::new("/root/Code.luau"));
                    let code = tree.get_instance(*ids.iter().next().unwrap()).unwrap();
                    if code.properties().get(&rbx_dom_weak::ustr("Source"))
                        == Some(&rbx_dom_weak::types::Variant::String("return 42".into()))
                    {
                        break;
                    }
                }
            })
            .await
            .expect("Normal source watch reload timed out");
        });
    }

    #[test]
    fn watch_reload_keeps_build_pool_and_discovers_new_initializer() {
        use memofs::{InMemoryFs, VfsEvent, VfsSnapshot};
        for workers in [1, 4] {
            let mut imfs = InMemoryFs::new();
            imfs.load_snapshot(
                "/root",
                VfsSnapshot::dir([
                    (
                        "default.project.json",
                        VfsSnapshot::file(
                            r#"{ "name": "WatchTest", "tree": { "$path": "Source" } }"#,
                        ),
                    ),
                    (
                        "Source",
                        VfsSnapshot::dir((0..16).map(|index| {
                            (
                                format!("Child{index:02}.luau"),
                                VfsSnapshot::file("return 1"),
                            )
                        })),
                    ),
                ]),
            )
            .unwrap();
            let pool = Arc::new(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .build()
                    .unwrap(),
            );
            let session = ServeSession::new_with_pool(
                Vfs::new(imfs.clone()),
                "/root",
                Some(Arc::clone(&pool)),
            )
            .unwrap();
            assert_eq!(Arc::strong_count(&pool), 2);
            let cursor = session.message_queue().cursor();
            imfs.load_snapshot("/root/Source/init.luau", VfsSnapshot::file("return 42"))
                .unwrap();
            imfs.raise_event(VfsEvent::Create("/root/Source/init.luau".into()));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    session.message_queue().subscribe(cursor),
                )
                .await
                .expect("Watch reload timed out")
                .unwrap();
            });
            let tree = session.tree();
            assert_eq!(tree.inner().root().class.as_str(), "ModuleScript");
            assert_eq!(tree.inner().root().children().len(), 16);
            drop(tree);
            drop(session);
            assert_eq!(Arc::strong_count(&pool), 1);
        }
    }

    #[test]
    fn relocation_recovery_precedes_the_initial_snapshot_and_invalidates_cached_paths() {
        use memofs::{InMemoryFs, VfsSnapshot};
        let mut backend = InMemoryFs::new();
        backend
            .load_snapshot(
                "/project",
                VfsSnapshot::dir([
                    (
                        "default.project.json",
                        VfsSnapshot::file(r#"{"name":"Test","tree":{"$path":"Source"}}"#),
                    ),
                    (
                        "Source",
                        VfsSnapshot::dir([("Code.luau", VfsSnapshot::file("return 42"))]),
                    ),
                ]),
            )
            .unwrap();
        let vfs = Vfs::new(backend);
        vfs.set_watch_enabled(false);
        let source = PathBuf::from("/project/Source/Code.luau");
        let destination = PathBuf::from("/project/Source/Renamed.luau");
        let expected = vfs.snapshot_tree(&source).unwrap();
        vfs.read(&source).unwrap();
        vfs.read_dir("/project/Source").unwrap();
        let journal = PathBuf::from("/project/.rojo/relocations/interrupted");
        vfs.create_dir_all(&journal).unwrap();
        let relocation = crate::syncback::relocation_transaction::Relocation {
            source: source.clone(),
            destination: destination.clone(),
            expected: expected.clone(),
        };
        vfs.write(
            journal.join("intent.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "moves": [relocation]
            }))
            .unwrap(),
        )
        .unwrap();
        vfs.move_no_replace(&source, &journal.join("stage-0"), &expected)
            .unwrap();
        let session = ServeSession::new(vfs, "/project").unwrap();
        let tree = session.tree();
        let child = tree.get_instance(tree.root().children()[0]).unwrap();
        assert_eq!(child.name(), "Code");
        assert_eq!(
            child.properties().get(&rbx_dom_weak::ustr("Source")),
            Some(&rbx_dom_weak::types::Variant::String("return 42".into()))
        );
        assert!(!tree.get_ids_at_path(&source).is_empty());
        assert!(tree.get_ids_at_path(&destination).is_empty());
        assert!(session.vfs().metadata(&journal).is_err());
    }

    #[test]
    fn tree_is_keyed_by_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.project.json"),
            r#"{ "name": "test", "tree": { "$className": "Folder" } }"#,
        )
        .unwrap();

        // `std::fs::canonicalize` yields a verbatim path on Windows.
        // On other platforms it simply resolves the path (e.g. symlinks),
        // which the session must also handle.
        let start_path = std::fs::canonicalize(dir.path()).unwrap();

        let vfs = Vfs::new(StdBackend::new().unwrap());
        let session = ServeSession::new(vfs, &start_path).unwrap();

        let project_file = start_path.join("default.project.json");
        let canonical = session.vfs().canonicalize(&project_file).unwrap();

        assert!(
            !session.tree().get_ids_at_path(&canonical).is_empty(),
            "project file {} should be tracked in the tree under its canonical \
             path {}, matching what the watcher reports",
            project_file.display(),
            canonical.display(),
        );
    }
}
