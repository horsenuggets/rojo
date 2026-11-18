//! Recoverable, no-clobber filesystem relocations.
//!
//! The coordinator must call `recover` for outstanding journals before taking session
//! snapshots. Supply a fresh transaction directory under a project-owned, ignored cache,
//! outside every watched source root, on the same filesystem as the sources. The parent
//! must already exist and be durable. Journals contain source bytes; protect them like the
//! project itself. Serialize execute/recover calls for that cache across processes.
//!
//! Every source is staged before any destination is installed. The immutable intent is
//! flushed before staging; a flushed commit marker is written after all installs. Without
//! that marker recovery rolls back, never replacing an occupied original path. Ambiguous
//! or edited data is left in place with the journal for manual resolution. Terminal journals
//! are retained, so the caller can enumerate them and retire them under its cache policy.
//!
//! This is not globally atomic to editors. Source validation is not an OS compare-and-swap;
//! an edit can race validation and follow the moved inode. Post-move validation detects
//! content changes, but cannot detect identical-byte replacement (ABA). Recovery preflights
//! canonical ancestors and package authority; adversarial substitutions racing those checks
//! are outside this contract. Unix durability relies
//! on filesystem fsync semantics; Windows flushes files and uses write-through moves, but
//! does not promise power-loss ordering of directory creation. In-memory durability lasts
//! only for the lifetime of the shared backend.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

use memofs::{IoResultExt, Vfs, VfsLock, VfsSnapshot};
use serde::{Deserialize, Serialize};

const JOURNAL_VERSION: u32 = 1;
const STRUCTURAL_VERSION: u32 = 2;
const COMMIT: &[u8] = b"committed\n";
const ROLLBACK: &[u8] = b"rolled back\n";

/// One whole-tree relocation, including sidecars represented as separate entries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relocation {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub expected: VfsSnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    version: u32,
    moves: Vec<Relocation>,
    #[serde(default)]
    absent_paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ownership: Option<ReplacementOwnership>,
}

/// Replaces disjoint original trees with one directory. Each entry's destination is
/// an immediate child of `destination`; its expected bytes also define the output.
/// Originals remain staged until the commit is durable, unlike ordinary relocations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Promotion {
    pub destination: PathBuf,
    pub originals: Vec<Relocation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PromotionJournal {
    version: u32,
    kind: String,
    promotion: Promotion,
    #[serde(default)]
    absent_paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ownership: Option<ReplacementOwnership>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    Committed,
    RolledBack,
}

/// Generated scratch output is not an authoritative source move. Package installs
/// use absence-checked replacements, keeping scratch bytes outside source ownership.
pub(crate) fn execute_staged(
    vfs: &Vfs,
    directory: &Path,
    moves: Vec<Relocation>,
) -> io::Result<()> {
    let Some(ownership) = capture_structural_ownership(vfs, directory, &moves)? else {
        return execute(vfs, directory, moves, Vec::new());
    };
    let staging = ownership.policy.root.join(".rojo/staging");
    let mut entries = Vec::new();
    {
        let mut fs = vfs.lock();
        validate(&mut fs, directory, &moves, true)?;
        for entry in moves {
            if !entry.source.starts_with(&staging)
                || !ownership.policy.is_local_source(&entry.destination)
            {
                return Err(recovery_refused(
                    directory,
                    "Package output must originate in private scratch staging",
                ));
            }
            require_expected(&fs, &entry.source, &entry.expected)?;
            entries.push(Replacement {
                path: entry.destination,
                expected: None,
                desired: Some(entry.expected),
            });
        }
    }
    execute_replacements(vfs, directory, entries)
}

/// Execute disjoint moves. Paths must be absolute and lexically normalized. All source
/// and destination trees must be mutually disjoint (cycles and nested moves are rejected).
/// Parent directories must exist; create-directory/promotion plans are not implemented.
/// A failure attempts rollback; if that fails, the error identifies the retained journal.
/// No path is copied across filesystems. A cache on another filesystem fails safely.
pub fn execute(
    vfs: &Vfs,
    journal_dir: &Path,
    moves: Vec<Relocation>,
    absent_paths: Vec<PathBuf>,
) -> io::Result<()> {
    let ownership = capture_structural_ownership(vfs, journal_dir, &moves)?;
    let mut fs = vfs.lock();
    validate_structural_ownership(&mut fs, journal_dir, ownership.as_ref(), &moves)?;
    validate(&mut fs, journal_dir, &moves, true)?;
    require_absent(&fs, &absent_paths)?;
    for relocation in &moves {
        require_expected(&fs, &relocation.source, &relocation.expected)?;
        if fs
            .snapshot_tree(&relocation.destination)
            .with_not_found()?
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Relocation destination is occupied",
            ));
        }
    }
    let journal = Journal {
        version: STRUCTURAL_VERSION,
        moves,
        absent_paths,
        ownership,
    };
    let bytes = serde_json::to_vec(&journal).map_err(io::Error::other)?;
    publish_intent(&mut fs, journal_dir, &bytes)?;
    execute_prepared(&mut fs, journal_dir, &journal)
}

fn execute_prepared(fs: &mut VfsLock<'_>, journal_dir: &Path, journal: &Journal) -> io::Result<()> {
    let result = (|| {
        require_absent(fs, &journal.absent_paths)?;
        validate_structural_ownership(fs, journal_dir, journal.ownership.as_ref(), &journal.moves)?;
        for (index, relocation) in journal.moves.iter().enumerate() {
            flush_tree(fs, &relocation.source)?;
            transfer(
                fs,
                &relocation.source,
                &stage(journal_dir, index),
                &relocation.expected,
            )?;
        }
        validate_prepared_structure(fs, journal_dir)?;
        for (index, relocation) in journal.moves.iter().enumerate() {
            transfer(
                fs,
                &stage(journal_dir, index),
                &relocation.destination,
                &relocation.expected,
            )?;
        }
        commit(fs, journal_dir, &journal.absent_paths, &journal.moves)
    })();
    if let Err(error) = result {
        match recover_locked(fs, journal_dir) {
            Ok(Recovery::Committed) => return Ok(()),
            Ok(Recovery::RolledBack) => return Err(error),
            Err(recovery_error) => return Err(io::Error::other(format!(
                "Relocation failed ({error}); recovery requires attention ({recovery_error}). Journal retained at {}",
                journal_dir.display()
            ))),
        }
    }
    Ok(())
}

/// Recover one transaction, idempotently. Call before collecting session snapshots.
/// Missing/corrupt intents or markers fail closed without removing any data. Package
/// writes require durable ownership evidence matching the current Wally generation.
/// Version-two structural records persist this witness before staging. Untagged legacy
/// application journals remain recoverable unless current declarations protect their paths;
/// they cannot prove historical package ownership after those declarations were removed.
/// This API does not choose a cache location on the caller's behalf.
pub fn recover(vfs: &Vfs, journal_dir: &Path) -> io::Result<Recovery> {
    // Validate current authority before acquiring the VFS lock. The locked recovery
    // checks the same durable evidence again before performing any mutation.
    require_canonical_parent(&mut vfs.lock(), &journal_dir.join("intent.json"))?;
    let bytes = read_file(&vfs.lock(), &journal_dir.join("intent.json"))?;
    let intent: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if intent.get("kind").and_then(serde_json::Value::as_str) == Some("replacement") {
        let journal: ReplacementJournal =
            serde_json::from_value(intent).map_err(io::Error::other)?;
        let ownership = journal.ownership.as_ref().ok_or_else(|| {
            recovery_refused(
                journal_dir,
                "Legacy package journal has no durable ownership evidence",
            )
        })?;
        ownership
            .policy
            .check_generation(vfs)
            .map_err(|error| recovery_refused(journal_dir, &format!("{error:#}")))?;
        for entry in &journal.entries {
            crate::package_sources::contained(vfs, &ownership.policy.root, &entry.path)
                .map_err(|error| recovery_refused(journal_dir, &format!("{error:#}")))?;
            if let Some(reason) = ownership.policy.blocked(vfs, &entry.path, true) {
                return Err(recovery_refused(journal_dir, &reason));
            }
        }
    } else if intent
        .get("ownership")
        .is_none_or(serde_json::Value::is_null)
    {
        // Old moves/promotions carry no package generation witness. Keep ordinary
        // application recovery working, but never infer package ownership from one.
        let paths: Vec<PathBuf> = if intent.get("kind").is_some() {
            let journal: PromotionJournal =
                serde_json::from_value(intent).map_err(io::Error::other)?;
            std::iter::once(journal.promotion.destination)
                .chain(
                    journal
                        .promotion
                        .originals
                        .into_iter()
                        .flat_map(|entry| [entry.source, entry.destination]),
                )
                .collect()
        } else {
            let journal: Journal = serde_json::from_value(intent).map_err(io::Error::other)?;
            journal
                .moves
                .into_iter()
                .flat_map(|entry| [entry.source, entry.destination])
                .collect()
        };
        for ancestor in journal_dir.ancestors().skip(1) {
            if let Some(policy) = crate::package_sources::PackageSources::load(vfs, ancestor)
                .map_err(io::Error::other)?
            {
                for path in &paths {
                    if policy.is_local_source(path) || policy.blocked(vfs, path, true).is_some() {
                        return Err(recovery_refused(
                            journal_dir,
                            "Move or promotion journal has no package ownership witness",
                        ));
                    }
                }
                break;
            }
        }
    }
    recover_locked(&mut vfs.lock(), journal_dir)
}

fn recover_locked(fs: &mut VfsLock<'_>, journal_dir: &Path) -> io::Result<Recovery> {
    let bytes = read_file(fs, &journal_dir.join("intent.json"))?;
    let intent: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if intent.get("kind").and_then(serde_json::Value::as_str) == Some("replacement") {
        let journal: ReplacementJournal =
            serde_json::from_value(intent).map_err(io::Error::other)?;
        return recover_replacement(fs, journal_dir, &journal);
    }
    if intent.get("kind").is_some() {
        let journal: PromotionJournal = serde_json::from_value(intent).map_err(io::Error::other)?;
        if ![JOURNAL_VERSION, STRUCTURAL_VERSION].contains(&journal.version)
            || journal.kind != "promotion"
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unknown promotion journal",
            ));
        }
        validate_structural_ownership(
            fs,
            journal_dir,
            journal.ownership.as_ref(),
            &journal.promotion.originals,
        )?;
        return recover_promotion(fs, journal_dir, &journal.promotion);
    }
    // Keep recognizing version-one untagged move journals created by older sessions.
    let journal: Journal = serde_json::from_value(intent).map_err(io::Error::other)?;
    if ![JOURNAL_VERSION, STRUCTURAL_VERSION].contains(&journal.version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unknown relocation journal version",
        ));
    }
    validate_structural_ownership(fs, journal_dir, journal.ownership.as_ref(), &journal.moves)?;
    validate(fs, journal_dir, &journal.moves, false)?;
    let committed = marker(fs, journal_dir, "commit", COMMIT)?;
    let rolled_back = marker(fs, journal_dir, "rollback", ROLLBACK)?;
    if committed && rolled_back {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Relocation journal contains contradictory terminal markers",
        ));
    }
    if committed {
        // A visible commit can precede the final directory flush failing. Retry it
        // before acknowledging success, so execute never reports an unflushed commit.
        fs.sync_path(&journal_dir.join("commit"))?;
        fs.sync_path(journal_dir)?;
        return Ok(Recovery::Committed);
    }
    if rolled_back {
        fs.sync_path(&journal_dir.join("rollback"))?;
        fs.sync_path(journal_dir)?;
        return Ok(Recovery::RolledBack);
    }
    let mut conflicts = Vec::new();
    for (index, relocation) in journal.moves.iter().enumerate().rev() {
        let result = (|| {
            let staged = stage(journal_dir, index);
            let source = fs.snapshot_tree(&relocation.source).with_not_found()?;
            let staged_state = fs.snapshot_tree(&staged).with_not_found()?;
            if staged_state.is_some() {
                // Even an identical original may be an intervening editor creation.
                // Never remove it or overwrite it to make rollback succeed.
                if source.is_some() {
                    return Err(conflict(&relocation.source));
                }
                transfer(fs, &staged, &relocation.source, &relocation.expected)?;
            } else if let Some(source) = source {
                if source != relocation.expected {
                    return Err(conflict(&relocation.source));
                }
                // Two occupied endpoints cannot prove that rollback completed,
                // even when the original was recreated with identical bytes.
                if fs
                    .snapshot_tree(&relocation.destination)
                    .with_not_found()?
                    .is_some()
                {
                    return Err(conflict(&relocation.destination));
                }
                // Untouched, or already restored by an earlier recovery attempt.
                // Retry durability work in case that recovery stopped after rename.
                flush_tree(fs, &relocation.source)?;
                fs.sync_path(relocation.source.parent().unwrap())?;
                fs.sync_path(relocation.destination.parent().unwrap())?;
                fs.sync_path(journal_dir)?;
            } else {
                // Covers a crash after installation but before commit. Moving directly
                // back also covers a crash during a previous rollback.
                transfer(
                    fs,
                    &relocation.destination,
                    &relocation.source,
                    &relocation.expected,
                )?;
            }
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = result {
            conflicts.push(format!("{} ({error})", relocation.source.display()));
        }
    }
    if !conflicts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "Recovery preserved unresolved paths {}",
                conflicts.join(", ")
            ),
        ));
    }
    persist(fs, journal_dir, "rollback", ROLLBACK)?;
    Ok(Recovery::RolledBack)
}

/// Execute a promotion under the same external cache lock used for `execute` and
/// `recover`. This low-level API deliberately does not acquire that process lock.
/// The coordinator must wrap it at the relocation wrapper's shared locking boundary.
pub fn execute_promotion(
    vfs: &Vfs,
    journal_dir: &Path,
    promotion: Promotion,
    absent_paths: Vec<PathBuf>,
) -> io::Result<()> {
    let ownership = capture_structural_ownership(vfs, journal_dir, &promotion.originals)?;
    let mut fs = vfs.lock();
    validate_structural_ownership(
        &mut fs,
        journal_dir,
        ownership.as_ref(),
        &promotion.originals,
    )?;
    let output = validate_promotion(&mut fs, journal_dir, &promotion, true)?;
    require_absent(&fs, &absent_paths)?;
    for entry in &promotion.originals {
        require_expected(&fs, &entry.source, &entry.expected)?;
    }
    if fs
        .snapshot_tree(&promotion.destination)
        .with_not_found()?
        .is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Promotion destination is occupied",
        ));
    }
    let journal = PromotionJournal {
        version: STRUCTURAL_VERSION,
        kind: "promotion".to_owned(),
        promotion,
        absent_paths,
        ownership,
    };
    publish_intent(
        &mut fs,
        journal_dir,
        &serde_json::to_vec(&journal).map_err(io::Error::other)?,
    )?;
    execute_promotion_prepared(
        &mut fs,
        journal_dir,
        &journal.promotion,
        &output,
        &journal.absent_paths,
    )
}

fn validate_prepared_structure(fs: &mut VfsLock<'_>, directory: &Path) -> io::Result<()> {
    let bytes = read_file(fs, &directory.join("intent.json"))?;
    let intent: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if intent.get("kind").is_some() {
        let journal: PromotionJournal = serde_json::from_value(intent).map_err(io::Error::other)?;
        validate_structural_ownership(
            fs,
            directory,
            journal.ownership.as_ref(),
            &journal.promotion.originals,
        )
    } else {
        let journal: Journal = serde_json::from_value(intent).map_err(io::Error::other)?;
        validate_structural_ownership(fs, directory, journal.ownership.as_ref(), &journal.moves)
    }
}

fn execute_promotion_prepared(
    fs: &mut VfsLock<'_>,
    journal_dir: &Path,
    promotion: &Promotion,
    output: &VfsSnapshot,
    absent_paths: &[PathBuf],
) -> io::Result<()> {
    let result = (|| {
        require_absent(fs, absent_paths)?;
        validate_prepared_structure(fs, journal_dir)?;
        materialize(fs, &journal_dir.join("output"), output)?;
        flush_tree(fs, &journal_dir.join("output"))?;
        fs.sync_path(journal_dir)?;
        validate_prepared_structure(fs, journal_dir)?;
        for (index, entry) in promotion.originals.iter().enumerate() {
            flush_tree(fs, &entry.source)?;
            transfer(
                fs,
                &entry.source,
                &stage(journal_dir, index),
                &entry.expected,
            )?;
        }
        for (index, entry) in promotion.originals.iter().enumerate() {
            let name = entry.destination.file_name().unwrap();
            copy_tree_permissions(
                fs,
                &stage(journal_dir, index),
                &journal_dir.join("output").join(name),
                &entry.expected,
            )?;
        }
        flush_tree(fs, &journal_dir.join("output"))?;
        validate_prepared_structure(fs, journal_dir)?;
        transfer(
            fs,
            &journal_dir.join("output"),
            &promotion.destination,
            output,
        )?;
        commit(fs, journal_dir, absent_paths, &promotion.originals)
    })();
    if let Err(error) = result {
        return match recover_locked(fs, journal_dir) {
            Ok(Recovery::Committed) => Ok(()),
            Ok(Recovery::RolledBack) => Err(error),
            Err(recovery) => Err(io::Error::other(format!(
                "Promotion failed ({error}); recovery requires attention ({recovery}). Journal retained at {}",
                journal_dir.display()
            ))),
        };
    }
    Ok(())
}

fn validate_promotion(
    fs: &mut VfsLock<'_>,
    journal_dir: &Path,
    promotion: &Promotion,
    check_parents: bool,
) -> io::Result<VfsSnapshot> {
    // Validate original endpoints and the output root using the ordinary disjoint-path
    // rules, not the nested output children (whose parent does not exist yet).
    let mut paths = vec![journal_dir, promotion.destination.as_path()];
    paths.extend(
        promotion
            .originals
            .iter()
            .map(|entry| entry.source.as_path()),
    );
    validate_paths(fs, &paths, check_parents)?;
    let mut children = std::collections::BTreeMap::new();
    for entry in &promotion.originals {
        if entry.destination.parent() != Some(promotion.destination.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Promotion output entries must be immediate children",
            ));
        }
        let name = entry
            .destination
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name != "." && *name != ".." && !name.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Invalid promotion output name")
            })?;
        if children
            .insert(name.to_owned(), entry.expected.clone())
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Promotion output entries collide",
            ));
        }
    }
    if children.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Promotion has no originals",
        ));
    }
    Ok(VfsSnapshot::Dir { children })
}

fn copy_tree_permissions(
    fs: &mut VfsLock<'_>,
    source: &Path,
    destination: &Path,
    snapshot: &VfsSnapshot,
) -> io::Result<()> {
    if let VfsSnapshot::Dir { children } = snapshot {
        for (name, child) in children {
            copy_tree_permissions(fs, &source.join(name), &destination.join(name), child)?;
        }
    }
    // Apply directories last so restrictive modes do not prevent copying children.
    fs.copy_permissions(source, destination)
}

fn materialize(fs: &mut VfsLock<'_>, path: &Path, snapshot: &VfsSnapshot) -> io::Result<()> {
    match snapshot {
        VfsSnapshot::File { contents } => fs.write_atomic(path, contents, None),
        VfsSnapshot::Dir { children } => {
            fs.create_dir(path)?;
            for (name, child) in children {
                // Snapshots deserialized from journals are untrusted path inputs too.
                if Path::new(name).components().count() != 1
                    || name == "."
                    || name == ".."
                    || name.contains(['/', '\\'])
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid snapshot child name",
                    ));
                }
                materialize(fs, &path.join(name), child)?;
            }
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unknown snapshot type",
        )),
    }
}

// Materialization may stop midway. Only a byte-identical subset of the intended
// output can safely be retired; unexpected edits inside the private journal survive.
fn output_subset(actual: &VfsSnapshot, expected: &VfsSnapshot) -> bool {
    match (actual, expected) {
        (VfsSnapshot::Dir { children: actual }, VfsSnapshot::Dir { children: expected }) => {
            actual.iter().all(|(name, child)| {
                expected
                    .get(name)
                    .is_some_and(|expected| output_subset(child, expected))
            })
        }
        _ => actual == expected,
    }
}

fn recover_promotion(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    promotion: &Promotion,
) -> io::Result<Recovery> {
    let output = validate_promotion(fs, directory, promotion, false)?;
    let committed = marker(fs, directory, "commit", COMMIT)?;
    let rolled_back = marker(fs, directory, "rollback", ROLLBACK)?;
    if committed && rolled_back {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Promotion journal contains contradictory terminal markers",
        ));
    }
    for name in ["output", "reclaimed"] {
        if let Some(actual) = fs.snapshot_tree(&directory.join(name)).with_not_found()? {
            if !output_subset(&actual, &output) {
                return Err(conflict(&directory.join(name)));
            }
        }
    }
    if committed {
        for (index, entry) in promotion.originals.iter().enumerate() {
            require_expected(fs, &stage(directory, index), &entry.expected)?;
        }
        fs.sync_path(&directory.join("commit"))?;
        fs.sync_path(directory)?;
        return Ok(Recovery::Committed);
    }
    if rolled_back {
        for index in 0..promotion.originals.len() {
            if fs
                .snapshot_tree(&stage(directory, index))
                .with_not_found()?
                .is_some()
            {
                return Err(conflict(&stage(directory, index)));
            }
        }
        fs.sync_path(&directory.join("rollback"))?;
        fs.sync_path(directory)?;
        return Ok(Recovery::RolledBack);
    }
    if fs
        .snapshot_tree(&promotion.destination)
        .with_not_found()?
        .is_some()
    {
        // A second output endpoint makes ownership ambiguous even with identical bytes.
        if fs
            .snapshot_tree(&directory.join("output"))
            .with_not_found()?
            .is_some()
            || fs
                .snapshot_tree(&directory.join("reclaimed"))
                .with_not_found()?
                .is_some()
        {
            return Err(conflict(&promotion.destination));
        }
        // Reclaim instead of deleting. If validation or a flush fails after rename,
        // the next recovery can find this endpoint and resume without losing bytes.
        transfer(
            fs,
            &promotion.destination,
            &directory.join("reclaimed"),
            &output,
        )?;
    }
    for (index, entry) in promotion.originals.iter().enumerate().rev() {
        let staged = stage(directory, index);
        if fs.snapshot_tree(&staged).with_not_found()?.is_some() {
            if fs.snapshot_tree(&entry.source).with_not_found()?.is_some() {
                return Err(conflict(&entry.source));
            }
            transfer(fs, &staged, &entry.source, &entry.expected)?;
        } else {
            require_expected(fs, &entry.source, &entry.expected)?;
            flush_tree(fs, &entry.source)?;
            fs.sync_path(entry.source.parent().unwrap())?;
        }
    }
    persist(fs, directory, "rollback", ROLLBACK)?;
    Ok(Recovery::RolledBack)
}

/// One disjoint source tree replacement. None represents absence, not an empty file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Replacement {
    pub path: PathBuf,
    pub expected: Option<VfsSnapshot>,
    pub desired: Option<VfsSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReplacementJournal {
    version: u32,
    kind: String,
    entries: Vec<Replacement>,
    // Version-one journals remain readable, but missing package authority is never
    // permission to roll back. Preserve their backups for manual resolution.
    #[serde(default)]
    ownership: Option<ReplacementOwnership>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReplacementOwnership {
    policy: crate::package_sources::PackageSources,
    authority: Vec<(PathBuf, VfsSnapshot)>,
}

impl ReplacementOwnership {
    fn capture(vfs: &Vfs, directory: &Path) -> io::Result<Self> {
        let root = directory
            .ancestors()
            .skip(1)
            .find(|ancestor| {
                vfs.metadata(ancestor.join("wally.toml"))
                    .with_not_found()
                    .is_ok_and(|value| value.is_some())
            })
            .ok_or_else(|| {
                recovery_refused(
                    directory,
                    "Package writes require authoritative Wally manifests",
                )
            })?;
        let policy = crate::package_sources::PackageSources::load(vfs, root)
            .map_err(io::Error::other)?
            .ok_or_else(|| recovery_refused(directory, "Missing package ownership"))?;
        policy.check_generation(vfs).map_err(io::Error::other)?;
        let mut paths = vec![root.join("wally.toml"), root.join("wally.install.json")];
        for package in &policy.install.as_ref().unwrap().package_roots {
            if let Some(source) = &package.source_path {
                paths.push(root.join(source).join("wally.toml"));
            }
        }
        paths.sort();
        paths.dedup();
        let authority = paths
            .into_iter()
            .map(|path| {
                let snapshot = vfs.snapshot_tree(&path)?;
                Ok((path, snapshot))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self { policy, authority })
    }
}

fn recovery_refused(directory: &Path, reason: &str) -> io::Error {
    io::Error::other(format!(
        "Package recovery refused ({reason}). Journal and backups retained at {}. Restore the original Wally manifests, provenance, and canonical source directories before retrying, or copy the retained backups to a safe location and reconcile manually. Do not delete the journal or rerun Wally over unresolved sources",
        directory.display()
    ))
}

pub(crate) fn require_canonical_parent(fs: &mut VfsLock<'_>, path: &Path) -> io::Result<()> {
    let result = (|| {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Path has no parent"))?;
        if fs.normalize(parent)? != parent || !fs.metadata(parent)?.is_dir() {
            return Err(io::Error::other(
                "Parent is not an existing canonical directory",
            ));
        }
        Ok(())
    })();
    result.map_err(|error| io::Error::new(error.kind(), format!("Recovery cannot validate the parent of {} ({error}). Restore the original directories before retrying; journal and backups are retained", path.display())))
}

fn validate_replacement_ownership(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    journal: &ReplacementJournal,
) -> io::Result<()> {
    let ownership = journal.ownership.as_ref().ok_or_else(|| {
        recovery_refused(
            directory,
            "Legacy package journal has no durable ownership evidence",
        )
    })?;
    validate_ownership_evidence(fs, directory, ownership)?;
    for entry in &journal.entries {
        if !entry.path.starts_with(&ownership.policy.root) {
            return Err(recovery_refused(
                directory,
                "Recorded source escapes its package project",
            ));
        }
        require_canonical_parent(fs, &entry.path)?;
    }
    Ok(())
}

fn validate_ownership_evidence(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    ownership: &ReplacementOwnership,
) -> io::Result<()> {
    require_canonical_parent(fs, directory)?;
    if fs.metadata(directory).with_not_found()?.is_some() {
        require_canonical_parent(fs, &directory.join("intent.json"))?;
    }
    if !directory.starts_with(&ownership.policy.root)
        || ownership.policy.problem.is_some()
        || ownership.authority.is_empty()
    {
        return Err(recovery_refused(
            directory,
            "Invalid recorded package ownership",
        ));
    }
    // Preflight the entire batch, not only the next endpoint. Otherwise a later
    // unsafe ancestor could be discovered after earlier entries were restored.
    for (path, expected) in &ownership.authority {
        require_canonical_parent(fs, path)?;
        if fs.snapshot_tree(path).with_not_found()?.as_ref() != Some(expected) {
            return Err(recovery_refused(
                directory,
                "Wally generation changed since the transaction was prepared",
            ));
        }
    }
    Ok(())
}

pub(crate) fn capture_structural_ownership(
    vfs: &Vfs,
    directory: &Path,
    moves: &[Relocation],
) -> io::Result<Option<ReplacementOwnership>> {
    for ancestor in directory.ancestors().skip(1) {
        if let Some(policy) =
            crate::package_sources::PackageSources::load(vfs, ancestor).map_err(io::Error::other)?
        {
            let mut involved = false;
            for entry in moves {
                for path in [&entry.source, &entry.destination] {
                    if let Some(reason) = policy.blocked(vfs, path, true) {
                        return Err(recovery_refused(directory, &reason));
                    }
                    involved |= policy.is_local_source(path);
                }
            }
            return if involved {
                ReplacementOwnership::capture(vfs, directory).map(Some)
            } else {
                Ok(None)
            };
        }
    }
    Ok(None)
}

pub(crate) fn validate_structural_ownership(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    ownership: Option<&ReplacementOwnership>,
    moves: &[Relocation],
) -> io::Result<()> {
    let Some(ownership) = ownership else {
        return Ok(());
    };
    validate_ownership_evidence(fs, directory, ownership)?;
    for entry in moves {
        let owner = ownership.policy.local_source_owner(&entry.source);
        if owner.is_none() || owner != ownership.policy.local_source_owner(&entry.destination) {
            return Err(recovery_refused(
                directory,
                "Structural transaction crosses authoritative package sources",
            ));
        }
        for path in [&entry.source, &entry.destination] {
            // Promotion children do not have a parent until output installation.
            // Validate the closest existing ancestor, without following a substituted link.
            let mut candidate = path.as_path();
            while fs
                .metadata(candidate.parent().ok_or_else(|| conflict(path))?)
                .with_not_found()?
                .is_none()
            {
                candidate = candidate.parent().unwrap();
            }
            require_canonical_parent(fs, candidate)?;
        }
    }
    Ok(())
}

fn copy_retained_permissions(
    fs: &mut VfsLock<'_>,
    source: &Path,
    output: &Path,
    expected: &VfsSnapshot,
    desired: &VfsSnapshot,
) -> io::Result<()> {
    match (expected, desired) {
        (VfsSnapshot::Dir { children: before }, VfsSnapshot::Dir { children: after }) => {
            for (name, desired) in after {
                if let Some(expected) = before.get(name) {
                    copy_retained_permissions(
                        fs,
                        &source.join(name),
                        &output.join(name),
                        expected,
                        desired,
                    )?;
                }
            }
        }
        (VfsSnapshot::File { .. }, VfsSnapshot::File { .. }) => {}
        _ => return Ok(()),
    }
    fs.copy_permissions(source, output)
}

fn validate_replacements(directory: &Path, journal: &ReplacementJournal) -> io::Result<()> {
    if journal.version != JOURNAL_VERSION || journal.kind != "replacement" {
        return Err(io::Error::other("Unknown replacement journal"));
    }
    for (index, entry) in journal.entries.iter().enumerate() {
        if !entry.path.is_absolute()
            || entry
                .path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || entry.path.starts_with(directory)
            || directory.starts_with(&entry.path)
            || journal.entries[..index].iter().any(|other| {
                entry.path.starts_with(&other.path) || other.path.starts_with(&entry.path)
            })
        {
            return Err(io::Error::other(
                "Replacement paths must be normalized, disjoint, and outside their journal",
            ));
        }
    }
    Ok(())
}

/// Reuse relocation's durable staging, no-clobber moves, and recovery protocol
/// for a package batch. The VFS lock prevents publishing intermediate watcher states.
pub(crate) fn execute_replacements(
    vfs: &Vfs,
    directory: &Path,
    entries: Vec<Replacement>,
) -> io::Result<()> {
    let journal = ReplacementJournal {
        version: JOURNAL_VERSION,
        kind: "replacement".into(),
        entries,
        ownership: Some(ReplacementOwnership::capture(vfs, directory)?),
    };
    validate_replacements(directory, &journal)?;
    let ownership = journal.ownership.as_ref().unwrap();
    for entry in &journal.entries {
        if let Some(reason) = ownership.policy.blocked(vfs, &entry.path, true) {
            return Err(recovery_refused(directory, &reason));
        }
    }
    let mut fs = vfs.lock();
    validate_replacement_ownership(&mut fs, directory, &journal)?;
    for entry in &journal.entries {
        if fs.snapshot_tree(&entry.path).with_not_found()? != entry.expected {
            return Err(conflict(&entry.path));
        }
    }
    publish_intent(
        &mut fs,
        directory,
        &serde_json::to_vec(&journal).map_err(io::Error::other)?,
    )?;
    let result = (|| {
        for (index, entry) in journal.entries.iter().enumerate() {
            if let Some(desired) = &entry.desired {
                let output = directory.join(format!("output-{index}"));
                materialize(&mut fs, &output, desired)?;
                if let Some(expected) = &entry.expected {
                    copy_retained_permissions(&mut fs, &entry.path, &output, expected, desired)?;
                }
                flush_tree(&mut fs, &output)?;
                fs.sync_path(directory)?;
            }
        }
        validate_replacement_ownership(&mut fs, directory, &journal)?;
        for (index, entry) in journal.entries.iter().enumerate() {
            if let Some(expected) = &entry.expected {
                transfer(&mut fs, &entry.path, &stage(directory, index), expected)?;
            } else if fs.snapshot_tree(&entry.path).with_not_found()?.is_some() {
                return Err(conflict(&entry.path));
            }
        }
        validate_replacement_ownership(&mut fs, directory, &journal)?;
        for (index, entry) in journal.entries.iter().enumerate() {
            if let Some(desired) = &entry.desired {
                transfer(
                    &mut fs,
                    &directory.join(format!("output-{index}")),
                    &entry.path,
                    desired,
                )?;
            }
        }
        for entry in &journal.entries {
            if fs.snapshot_tree(&entry.path).with_not_found()? != entry.desired {
                return Err(conflict(&entry.path));
            }
        }
        validate_replacement_ownership(&mut fs, directory, &journal)?;
        persist(&mut fs, directory, "commit", COMMIT)
    })();
    if let Err(error) = result {
        match recover_replacement(&mut fs, directory, &journal) {
            Ok(Recovery::Committed) => return Ok(()),
            Ok(Recovery::RolledBack) => return Err(error),
            Err(recovery) => return Err(io::Error::other(format!("Package write failed ({error}). Recovery requires attention ({recovery}). Journal retained at {}", directory.display()))),
        }
    }
    Ok(())
}

fn recover_replacement(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    journal: &ReplacementJournal,
) -> io::Result<Recovery> {
    validate_replacements(directory, journal)?;
    validate_replacement_ownership(fs, directory, journal)?;
    let committed = marker(fs, directory, "commit", COMMIT)?;
    let rolled_back = marker(fs, directory, "rollback", ROLLBACK)?;
    if committed && rolled_back {
        return Err(io::Error::other(
            "Contradictory replacement journal markers",
        ));
    }
    for (index, entry) in journal.entries.iter().enumerate() {
        let backup_path = stage(directory, index);
        let backup = fs.snapshot_tree(&backup_path).with_not_found()?;
        if backup
            .as_ref()
            .is_some_and(|actual| Some(actual) != entry.expected.as_ref())
            || (committed && backup != entry.expected)
            || (rolled_back && backup.is_some())
        {
            return Err(conflict(&backup_path));
        }
        for prefix in ["output", "reclaimed"] {
            let path = directory.join(format!("{prefix}-{index}"));
            if let Some(actual) = fs.snapshot_tree(&path).with_not_found()? {
                if !entry
                    .desired
                    .as_ref()
                    .is_some_and(|desired| output_subset(&actual, desired))
                {
                    return Err(conflict(&path));
                }
            }
        }
    }
    if committed {
        fs.sync_path(&directory.join("commit"))?;
        fs.sync_path(directory)?;
        return Ok(Recovery::Committed);
    }
    if rolled_back {
        fs.sync_path(&directory.join("rollback"))?;
        fs.sync_path(directory)?;
        return Ok(Recovery::RolledBack);
    }
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        let backup = stage(directory, index);
        let output = directory.join(format!("output-{index}"));
        let reclaimed = directory.join(format!("reclaimed-{index}"));
        let staged = fs.snapshot_tree(&backup).with_not_found()?;
        let at_source = fs.snapshot_tree(&entry.path).with_not_found()?;
        let output_present = fs.snapshot_tree(&output).with_not_found()?.is_some();
        let reclaimed_present = fs.snapshot_tree(&reclaimed).with_not_found()?.is_some();
        if (staged.is_some() || entry.expected.is_none())
            && at_source.is_some()
            && !reclaimed_present
        {
            if output_present || at_source != entry.desired {
                return Err(conflict(&entry.path));
            }
            transfer(
                fs,
                &entry.path,
                &reclaimed,
                entry
                    .desired
                    .as_ref()
                    .ok_or_else(|| conflict(&entry.path))?,
            )?;
        }
        if let Some(expected) = &entry.expected {
            if let Some(staged) = staged {
                if &staged != expected || fs.snapshot_tree(&entry.path).with_not_found()?.is_some()
                {
                    return Err(conflict(&entry.path));
                }
                transfer(fs, &backup, &entry.path, expected)?;
            } else {
                require_expected(fs, &entry.path, expected)?;
            }
        } else if fs.snapshot_tree(&entry.path).with_not_found()?.is_some() {
            return Err(conflict(&entry.path));
        }
    }
    persist(fs, directory, "rollback", ROLLBACK)?;
    Ok(Recovery::RolledBack)
}

fn stage(journal_dir: &Path, index: usize) -> PathBuf {
    journal_dir.join(format!("stage-{index}"))
}

fn conflict(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "Relocation state is ambiguous or edited at {}",
            path.display()
        ),
    )
}

fn commit(
    fs: &mut VfsLock<'_>,
    journal_dir: &Path,
    absent_paths: &[PathBuf],
    originals: &[Relocation],
) -> io::Result<()> {
    require_absent(fs, absent_paths)?;
    validate_prepared_structure(fs, journal_dir)?;
    // Existing sources must become vacant after staging and stay vacant until commit.
    // An editor can recreate a sidecar even when it was present during planning.
    for original in originals {
        if fs
            .snapshot_tree(&original.source)
            .with_not_found()?
            .is_some()
        {
            return Err(conflict(&original.source));
        }
    }
    persist(fs, journal_dir, "commit", COMMIT)
}

// Absence is a transaction precondition, not a permanent invariant. Recovery must
// still accept committed output after legitimate metadata edits made later.
fn require_absent(fs: &VfsLock<'_>, paths: &[PathBuf]) -> io::Result<()> {
    for path in paths {
        if fs.snapshot_tree(path).with_not_found()?.is_some() {
            return Err(conflict(path));
        }
    }
    Ok(())
}

fn require_expected(fs: &VfsLock<'_>, path: &Path, expected: &VfsSnapshot) -> io::Result<()> {
    if &fs.snapshot_tree(path)? != expected {
        return Err(conflict(path));
    }
    Ok(())
}

fn transfer(
    fs: &mut VfsLock<'_>,
    source: &Path,
    destination: &Path,
    expected: &VfsSnapshot,
) -> io::Result<()> {
    fs.move_no_replace(source, destination, expected)?;
    // If validation or flushing fails after the move, retain the moved tree. Recovery
    // discovers its location from the immutable intent, not a possibly missing receipt.
    require_expected(fs, destination, expected)?;
    flush_tree(fs, destination)?;
    fs.sync_path(destination.parent().unwrap())?;
    fs.sync_path(source.parent().unwrap())
}

fn flush_tree(fs: &mut VfsLock<'_>, path: &Path) -> io::Result<()> {
    // Capture once, without installing watches on the journal's staged trees.
    // snapshot_tree rejects links and special files, including nested ones.
    fn flush(fs: &mut VfsLock<'_>, path: &Path, snapshot: &VfsSnapshot) -> io::Result<()> {
        if let VfsSnapshot::Dir { children } = snapshot {
            for (name, child) in children {
                flush(fs, &path.join(name), child)?;
            }
        }
        fs.sync_path(path)
    }
    let snapshot = fs.snapshot_tree(path)?;
    flush(fs, path, &snapshot)
}

fn read_file(fs: &VfsLock<'_>, path: &Path) -> io::Result<Vec<u8>> {
    match fs.snapshot_tree(path)? {
        VfsSnapshot::File { contents } => Ok(contents),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Journal entry is not a regular file",
        )),
    }
}

fn publish_intent(fs: &mut VfsLock<'_>, journal_dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let cache = journal_dir.parent().unwrap();
    let preparation = cache.join(format!(".preparing-{}", uuid::Uuid::new_v4()));
    fs.create_dir(&preparation)?;
    fs.sync_path(cache)?;
    // This file is deliberately not an active intent yet. Interrupted writes can
    // only leave preparation data; no original is staged until publication succeeds.
    fs.write(preparation.join("intent.json"), bytes)?;
    fs.sync_path(&preparation.join("intent.json"))?;
    fs.sync_path(&preparation)?;
    let expected = fs.snapshot_tree(&preparation)?;
    fs.move_no_replace(&preparation, journal_dir, &expected)?;
    fs.sync_path(cache)
}

/// An unpublished directory cannot contain staged originals. Retire only recognized
/// complete intents or empty preparations; preserve incomplete or unexpected contents
/// without letting an interrupted preparation block startup recovery.
pub(crate) fn preparation_is_discardable(vfs: &Vfs, directory: &Path) -> io::Result<bool> {
    let VfsSnapshot::Dir { children } = vfs.snapshot_tree(directory)? else {
        return Ok(false);
    };
    if children.is_empty() {
        return Ok(true);
    }
    if children.len() != 1 {
        return Ok(false);
    }
    let Some(VfsSnapshot::File { contents }) = children.get("intent.json") else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(contents) else {
        return Ok(false);
    };
    let mut fs = vfs.lock();
    if value.get("kind").is_some() {
        let Ok(journal) = serde_json::from_value::<PromotionJournal>(value) else {
            return Ok(false);
        };
        Ok(
            serde_json::to_vec(&journal).map_err(io::Error::other)? == *contents
                && [JOURNAL_VERSION, STRUCTURAL_VERSION].contains(&journal.version)
                && journal.kind == "promotion"
                && validate_promotion(&mut fs, directory, &journal.promotion, false).is_ok(),
        )
    } else {
        let Ok(journal) = serde_json::from_value::<Journal>(value) else {
            return Ok(false);
        };
        Ok(
            serde_json::to_vec(&journal).map_err(io::Error::other)? == *contents
                && [JOURNAL_VERSION, STRUCTURAL_VERSION].contains(&journal.version)
                && validate(&mut fs, directory, &journal.moves, false).is_ok(),
        )
    }
}

fn persist(fs: &mut VfsLock<'_>, directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let path = directory.join(name);
    fs.write_atomic(&path, bytes, None)?;
    fs.sync_path(&path)?;
    fs.sync_path(directory)
}

fn marker(fs: &mut VfsLock<'_>, directory: &Path, name: &str, expected: &[u8]) -> io::Result<bool> {
    match read_file(fs, &directory.join(name)).with_not_found()? {
        None => Ok(false),
        Some(bytes) if bytes.as_slice() == expected => Ok(true),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Corrupt relocation marker",
        )),
    }
}

fn validate(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    moves: &[Relocation],
    check_parents: bool,
) -> io::Result<()> {
    let mut paths = vec![directory];
    for relocation in moves {
        paths.extend([
            relocation.source.as_path(),
            relocation.destination.as_path(),
        ]);
    }
    validate_paths(fs, &paths, check_parents)
}

fn validate_paths(fs: &mut VfsLock<'_>, paths: &[&Path], check_parents: bool) -> io::Result<()> {
    for (index, path) in paths.iter().enumerate() {
        if !fs.is_absolute_path(path)
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Relocation requires absolute normalized paths",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot relocate filesystem roots",
            )
        })?;
        // Recovery must revalidate ancestors too. A previously canonical path can
        // point outside the project after an editor replaces a parent with a link.
        require_canonical_parent(fs, path)?;
        if check_parents && (fs.normalize(parent)? != parent || !fs.metadata(parent)?.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Relocation parent must be a canonical existing directory",
            ));
        }
        for other in &paths[..index] {
            if path.starts_with(other) || other.starts_with(path) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Relocation paths overlap",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use memofs::{InMemoryFs, StdBackend, VfsBackend};

    fn fixture(real: bool) -> (Vfs, tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let fs = if real {
            Vfs::new(StdBackend::new().unwrap())
        } else {
            let mut backend = InMemoryFs::new();
            backend.create_dir_all(&root).unwrap();
            Vfs::new(backend)
        };
        fs.set_watch_enabled(false);
        fs.create_dir(root.join("src")).unwrap();
        fs.create_dir(root.join("dst")).unwrap();
        fs.create_dir(root.join("cache")).unwrap();
        (fs, temp, root)
    }

    fn package_fixture(fs: &Vfs, root: &Path) {
        let manifest = "[dependencies]\nFoo = \"^src\"\n";
        let local = "[package]\nname = \"scope/foo\"\nrealm = \"shared\"\n";
        fs.write(root.join("wally.toml"), manifest).unwrap();
        fs.write(root.join("src/wally.toml"), local).unwrap();
        fs.create_dir_all(root.join("Packages")).unwrap();
        fs.write(root.join("wally.install.json"), serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "manifest_hash": crate::package_sources::sha256(manifest.as_bytes()),
            "local_manifests": {"src/wally.toml": crate::package_sources::sha256(local.as_bytes())},
            "package_roots": [{"name": "scope/foo", "kind": "local", "realm": "shared", "install_path": "Packages/_Index/foo/foo", "source_path": "src"}],
            "generated_paths": ["Packages/Foo.luau"]
        })).unwrap()).unwrap();
    }

    #[test]
    fn new_package_move_and_promotion_journals_persist_ownership() {
        for promotion in [false, true] {
            let (fs, _temp, root) = fixture(true);
            package_fixture(&fs, &root);
            fs.write(root.join("src/Before.luau"), "before").unwrap();
            let destination = root.join("src/After");
            let entry = Relocation {
                source: root.join("src/Before.luau"),
                destination: if promotion {
                    destination.join("init.luau")
                } else {
                    destination.clone()
                },
                expected: VfsSnapshot::file("before"),
            };
            let directory = root.join("cache/tx");
            if promotion {
                execute_promotion(
                    &fs,
                    &directory,
                    Promotion {
                        destination,
                        originals: vec![entry],
                    },
                    Vec::new(),
                )
                .unwrap();
            } else {
                execute(&fs, &directory, vec![entry], Vec::new()).unwrap();
            }
            let saved: serde_json::Value =
                serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap()).unwrap();
            assert_eq!(saved["version"], STRUCTURAL_VERSION);
            assert!(saved["ownership"]["authority"].as_array().unwrap().len() >= 3);
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
        }
    }

    #[test]
    fn package_structural_recovery_checks_evidence_at_every_crash_boundary() {
        for promotion in [false, true] {
            // Intent, partial staging, full staging, partial installation, full
            // installation, and durable commit all have independently recoverable states.
            for boundary in 0..=5 {
                for changed in ["none", "root", "local", "provenance", "all", "edited"] {
                    let (fs, _temp, root) = fixture(true);
                    package_fixture(&fs, &root);
                    let directory = root.join("cache/tx");
                    let destination = root.join("src/After");
                    let entries: Vec<_> = ["One.luau", "Two.luau"]
                        .into_iter()
                        .map(|name| {
                            let source = root.join("src").join(name);
                            fs.write(&source, name).unwrap();
                            Relocation {
                                source,
                                destination: if promotion {
                                    destination.join(name)
                                } else {
                                    root.join("src").join(format!("New{name}"))
                                },
                                expected: VfsSnapshot::file(name),
                            }
                        })
                        .collect();
                    let ownership =
                        capture_structural_ownership(&fs, &directory, &entries).unwrap();
                    assert!(ownership.is_some());
                    let plan = Promotion {
                        destination: destination.clone(),
                        originals: entries.clone(),
                    };
                    let output = if promotion {
                        Some(validate_promotion(&mut fs.lock(), &directory, &plan, true).unwrap())
                    } else {
                        None
                    };
                    let bytes = if promotion {
                        serde_json::to_vec(&PromotionJournal {
                            version: STRUCTURAL_VERSION,
                            kind: "promotion".into(),
                            promotion: plan,
                            absent_paths: Vec::new(),
                            ownership,
                        })
                        .unwrap()
                    } else {
                        serde_json::to_vec(&Journal {
                            version: STRUCTURAL_VERSION,
                            moves: entries.clone(),
                            absent_paths: Vec::new(),
                            ownership,
                        })
                        .unwrap()
                    };
                    publish_intent(&mut fs.lock(), &directory, &bytes).unwrap();
                    if let Some(output) = &output {
                        materialize(&mut fs.lock(), &directory.join("output"), output).unwrap();
                    }
                    for (index, entry) in entries.iter().enumerate().take(boundary.min(2)) {
                        transfer(
                            &mut fs.lock(),
                            &entry.source,
                            &stage(&directory, index),
                            &entry.expected,
                        )
                        .unwrap();
                    }
                    if promotion && boundary >= 3 {
                        transfer(
                            &mut fs.lock(),
                            &directory.join("output"),
                            &destination,
                            output.as_ref().unwrap(),
                        )
                        .unwrap();
                    } else if !promotion {
                        for (index, entry) in entries
                            .iter()
                            .enumerate()
                            .take(boundary.saturating_sub(2).min(2))
                        {
                            transfer(
                                &mut fs.lock(),
                                &stage(&directory, index),
                                &entry.destination,
                                &entry.expected,
                            )
                            .unwrap();
                        }
                    }
                    if boundary == 5 {
                        persist(&mut fs.lock(), &directory, "commit", COMMIT).unwrap();
                    }
                    let authority = [
                        root.join("wally.toml"),
                        root.join("src/wally.toml"),
                        root.join("wally.install.json"),
                    ];
                    let saved: Vec<_> = authority
                        .iter()
                        .map(|path| fs.read(path).unwrap())
                        .collect();
                    for (index, path) in authority.iter().enumerate() {
                        if changed == "all" || changed == ["root", "local", "provenance"][index] {
                            fs.remove_file(path).unwrap();
                        }
                    }
                    if changed == "edited" {
                        fs.write(&authority[0], "[dependencies]\n").unwrap();
                    }
                    let before = fs.snapshot_tree(&root).unwrap();
                    let restarted = Vfs::new(StdBackend::new().unwrap());
                    restarted.set_watch_enabled(false);
                    if changed != "none" {
                        assert!(
                            recover(&restarted, &directory).is_err(),
                            "{promotion} {boundary} {changed}"
                        );
                        assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
                        for (path, bytes) in authority.iter().zip(saved) {
                            fs.write(path, bytes.as_slice()).unwrap();
                        }
                    }
                    let expected = if boundary == 5 {
                        Recovery::Committed
                    } else {
                        Recovery::RolledBack
                    };
                    assert_eq!(
                        recover(&restarted, &directory).unwrap(),
                        expected,
                        "{promotion} {boundary} {changed}"
                    );
                    assert_eq!(recover(&restarted, &directory).unwrap(), expected);
                    for entry in &entries {
                        let path = if boundary == 5 {
                            &entry.destination
                        } else {
                            &entry.source
                        };
                        assert_eq!(fs.snapshot_tree(path).unwrap(), entry.expected);
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_structural_recovery_rejects_substituted_ancestors_and_resumes() {
        for (promotion, endpoint) in [false, true].into_iter().flat_map(|promotion| {
            ["authority", "source", "destination"].map(|endpoint| (promotion, endpoint))
        }) {
            let (fs, _temp, root) = fixture(true);
            package_fixture(&fs, &root);
            fs.create_dir(root.join("src/input")).unwrap();
            fs.create_dir(root.join("src/output")).unwrap();
            let directory = root.join("cache/tx");
            let source = root.join("src/input/Before.luau");
            fs.write(&source, "before").unwrap();
            let destination = root.join("src/output/After");
            let entry = Relocation {
                source: source.clone(),
                destination: if promotion {
                    destination.join("init.luau")
                } else {
                    destination.clone()
                },
                expected: VfsSnapshot::file("before"),
            };
            let ownership =
                capture_structural_ownership(&fs, &directory, std::slice::from_ref(&entry))
                    .unwrap();
            let bytes = if promotion {
                serde_json::to_vec(&PromotionJournal {
                    version: STRUCTURAL_VERSION,
                    kind: "promotion".into(),
                    promotion: Promotion {
                        destination,
                        originals: vec![entry.clone()],
                    },
                    absent_paths: Vec::new(),
                    ownership,
                })
                .unwrap()
            } else {
                serde_json::to_vec(&Journal {
                    version: STRUCTURAL_VERSION,
                    moves: vec![entry.clone()],
                    absent_paths: Vec::new(),
                    ownership,
                })
                .unwrap()
            };
            publish_intent(&mut fs.lock(), &directory, &bytes).unwrap();
            transfer(
                &mut fs.lock(),
                &source,
                &stage(&directory, 0),
                &entry.expected,
            )
            .unwrap();
            let held = root.join("dst/held");
            let linked = root.join(match endpoint {
                "source" => "src/input",
                "destination" => "src/output",
                _ => "src",
            });
            std::fs::rename(&linked, &held).unwrap();
            std::os::unix::fs::symlink(&held, &linked).unwrap();
            let before = fs.snapshot_tree(&directory).unwrap();
            let held_before = fs.snapshot_tree(&held).unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            assert!(recover(&restarted, &directory).is_err());
            assert_eq!(fs.snapshot_tree(&directory).unwrap(), before);
            assert_eq!(fs.snapshot_tree(&held).unwrap(), held_before);
            std::fs::remove_file(&linked).unwrap();
            std::fs::rename(&held, &linked).unwrap();
            assert_eq!(
                recover(&restarted, &directory).unwrap(),
                Recovery::RolledBack
            );
            assert_eq!(fs.snapshot_tree(&source).unwrap(), entry.expected);
        }
    }

    #[test]
    fn package_relayout_recovery_preserves_exact_backups_until_authority_is_restored() {
        for boundary in 0..=2 {
            for changed in [
                "root",
                "local",
                "provenance",
                "all",
                "edited",
                "data",
                "missing",
            ] {
                let (fs, _temp, root) = fixture(true);
                package_fixture(&fs, &root);
                let project_path = root.join("default.project.json");
                fs.write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#)
                    .unwrap();
                let project = crate::Project::load_exact(&fs, &project_path, Some("Test")).unwrap();
                let directory =
                    super::super::relocation::relayout_directory(&project).join("interrupted");
                fs.create_dir_all(&directory).unwrap();
                let paths = [root.join("src/One.luau"), root.join("src/Two.luau")];
                for path in &paths {
                    fs.write(path, "before").unwrap();
                }
                let manifest = super::super::relocation::RelayoutManifest::prepare(
                    &fs,
                    &directory,
                    &paths,
                    &root.join("src"),
                )
                .unwrap();
                assert!(manifest.package.is_some());
                fs.write(
                    directory.join("manifest.json"),
                    serde_json::to_vec(&manifest).unwrap(),
                )
                .unwrap();
                for (index, path) in paths.iter().enumerate().take(boundary) {
                    transfer(
                        &mut fs.lock(),
                        path,
                        &directory.join(index.to_string()),
                        &VfsSnapshot::file("before"),
                    )
                    .unwrap();
                }
                let authority = [
                    root.join("wally.toml"),
                    root.join("src/wally.toml"),
                    root.join("wally.install.json"),
                ];
                let saved: Vec<_> = authority
                    .iter()
                    .map(|path| fs.read(path).unwrap())
                    .collect();
                for (index, path) in authority.iter().enumerate() {
                    if changed == "all" || changed == ["root", "local", "provenance"][index] {
                        fs.remove_file(path).unwrap();
                    }
                }
                if changed == "edited" {
                    fs.write(&authority[0], "[dependencies]\n").unwrap();
                }
                let data_path = if boundary == 0 {
                    paths[0].clone()
                } else {
                    directory.join("0")
                };
                if changed == "data" {
                    fs.write(&data_path, "edited").unwrap();
                }
                if changed == "missing" {
                    fs.remove_file(&data_path).unwrap();
                }
                let before = fs.snapshot_tree(&root).unwrap();
                let restarted = Vfs::new(StdBackend::new().unwrap());
                restarted.set_watch_enabled(false);
                assert!(crate::serve_session::ServeSession::new(
                    Vfs::new(StdBackend::new().unwrap()),
                    &root
                )
                .is_err());
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
                assert!(super::super::relocation::recover_relayouts(&restarted, &project).is_err());
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
                for (path, bytes) in authority.iter().zip(saved) {
                    fs.write(path, bytes.as_slice()).unwrap();
                }
                if matches!(changed, "data" | "missing") {
                    fs.write(&data_path, "before").unwrap();
                }
                super::super::relocation::recover_relayouts(&restarted, &project).unwrap();
                for path in &paths {
                    assert_eq!(fs.snapshot_tree(path).unwrap(), VfsSnapshot::file("before"));
                }
                assert!(!fs.exists(&directory).unwrap());
            }
        }
    }

    #[test]
    fn package_structural_execution_refuses_cross_source_moves_before_publication() {
        for promotion in [false, true] {
            let (fs, _temp, root) = fixture(false);
            package_fixture(&fs, &root);
            let source = root.join("src/Before.luau");
            fs.write(&source, "before").unwrap();
            let directory = root.join("cache/tx");
            let destination = root.join("dst/After");
            let entry = Relocation {
                source: source.clone(),
                destination: if promotion {
                    destination.join("init.luau")
                } else {
                    destination.clone()
                },
                expected: VfsSnapshot::file("before"),
            };
            let before = fs.snapshot_tree(&root).unwrap();
            let result = if promotion {
                execute_promotion(
                    &fs,
                    &directory,
                    Promotion {
                        destination,
                        originals: vec![entry],
                    },
                    Vec::new(),
                )
            } else {
                execute(&fs, &directory, vec![entry], Vec::new())
            };
            assert!(result.is_err());
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_relayout_recovery_rejects_source_and_output_directory_substitutions() {
        for output in [false, true] {
            let (fs, _temp, root) = fixture(true);
            package_fixture(&fs, &root);
            let project_path = root.join("default.project.json");
            fs.write(&project_path, r#"{"name":"Test","tree":{"$path":"src"}}"#)
                .unwrap();
            let project = crate::Project::load_exact(&fs, &project_path, Some("Test")).unwrap();
            let directory =
                super::super::relocation::relayout_directory(&project).join("interrupted");
            fs.create_dir_all(&directory).unwrap();
            fs.create_dir(root.join("src/output")).unwrap();
            let original = root.join("src/Before.luau");
            fs.write(&original, "before").unwrap();
            let manifest = super::super::relocation::RelayoutManifest::prepare(
                &fs,
                &directory,
                std::slice::from_ref(&original),
                &root.join("src/output"),
            )
            .unwrap();
            fs.write(
                directory.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            transfer(
                &mut fs.lock(),
                &original,
                &directory.join("0"),
                &VfsSnapshot::file("before"),
            )
            .unwrap();
            let linked = root.join(if output { "src/output" } else { "src" });
            let outside = tempfile::tempdir().unwrap();
            let held = outside.path().join("held");
            std::fs::rename(&linked, &held).unwrap();
            std::os::unix::fs::symlink(&held, &linked).unwrap();
            let before = fs.snapshot_tree(&directory).unwrap();
            let held_before = fs.snapshot_tree(&held).unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            assert!(super::super::relocation::recover_relayouts(&restarted, &project).is_err());
            assert_eq!(fs.snapshot_tree(&directory).unwrap(), before);
            assert_eq!(fs.snapshot_tree(&held).unwrap(), held_before);
            std::fs::remove_file(&linked).unwrap();
            std::fs::rename(&held, &linked).unwrap();
            super::super::relocation::recover_relayouts(&restarted, &project).unwrap();
            assert_eq!(
                fs.snapshot_tree(&original).unwrap(),
                VfsSnapshot::file("before")
            );
        }
    }

    #[test]
    fn generated_package_output_uses_absence_checked_replacements_not_cross_source_moves() {
        let (fs, _temp, root) = fixture(false);
        package_fixture(&fs, &root);
        fs.create_dir_all(root.join(".rojo/staging/new")).unwrap();
        let source = root.join(".rojo/staging/new/Code.luau");
        let destination = root.join("src/Code.luau");
        fs.write(&source, "new").unwrap();
        let entry = Relocation {
            source: source.clone(),
            destination: destination.clone(),
            expected: VfsSnapshot::file("new"),
        };
        let directory = root.join("cache/tx");
        execute_staged(&fs, &directory, vec![entry.clone()]).unwrap();
        assert_eq!(fs.snapshot_tree(&source).unwrap(), entry.expected);
        assert_eq!(fs.snapshot_tree(&destination).unwrap(), entry.expected);
        assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
        let occupied = root.join("cache/occupied");
        assert!(execute_staged(&fs, &occupied, vec![entry]).is_err());
        assert!(!fs.exists(&occupied).unwrap());
        assert_eq!(fs.read(&destination).unwrap().as_slice(), b"new");
    }

    #[test]
    fn legacy_package_recovery_preserves_backups_without_ownership() {
        let (fs, _temp, root) = fixture(false);
        package_fixture(&fs, &root);
        let directory = root.join("cache/legacy-package");
        fs.create_dir(&directory).unwrap();
        fs.write(stage(&directory, 0), "before").unwrap();
        fs.write(
            directory.join("intent.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "kind": "replacement", "entries": [{
                    "path": root.join("src/source.luau"),
                    "expected": VfsSnapshot::file("before"),
                    "desired": VfsSnapshot::file("after")
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let before = fs.snapshot_tree(&root).unwrap();
        let error = recover(&fs, &directory).unwrap_err().to_string();
        assert!(error.contains("no durable ownership evidence"));
        assert!(error.contains("reconcile manually"));
        assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
    }

    #[test]
    fn ordinary_recovery_in_package_projects_requires_no_package_ownership() {
        for (package_path, invalid_generation) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let (fs, _temp, root) = fixture(false);
            package_fixture(&fs, &root);
            if invalid_generation {
                fs.write(root.join("wally.install.json"), "invalid generation")
                    .unwrap();
            }
            let folder = if package_path { "src" } else { "dst" };
            let source = root.join(folder).join("Code.luau");
            let destination = root.join(folder).join("Renamed.luau");
            fs.write(&source, "before").unwrap();
            let expected = fs.snapshot_tree(&source).unwrap();
            let directory = root.join("cache/ordinary");
            intent(
                &fs,
                &directory,
                &[Relocation {
                    source: source.clone(),
                    destination,
                    expected: expected.clone(),
                }],
            );
            fs.move_no_replace(&source, &stage(&directory, 0), &expected)
                .unwrap();
            let before = fs.snapshot_tree(&root).unwrap();
            if package_path {
                assert!(recover(&fs, &directory).is_err());
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            } else {
                assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
                assert_eq!(fs.snapshot_tree(&source).unwrap(), expected);
            }
        }
    }

    #[test]
    fn package_recovery_refuses_changed_authority_after_restart() {
        for authority in ["wally.toml", "src/wally.toml", "wally.install.json"] {
            let (fs, _temp, root) = fixture(true);
            package_fixture(&fs, &root);
            let path = root.join("src/source.luau");
            let directory = root.join("cache/package-batch");
            fs.write(&path, "before").unwrap();
            execute_replacements(
                &fs,
                &directory,
                vec![Replacement {
                    path: path.clone(),
                    expected: Some(VfsSnapshot::file("before")),
                    desired: Some(VfsSnapshot::file("after")),
                }],
            )
            .unwrap();
            fs.remove_file(directory.join("commit")).unwrap();
            fs.write(root.join(authority), "changed authority").unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            let before = restarted.snapshot_tree(&root).unwrap();
            assert!(
                recover(&restarted, &directory).is_err(),
                "accepted changed {authority}"
            );
            assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[test]
    fn startup_preflights_legacy_relayout_ownership_before_restoring_backups() {
        for package_path in [false, true] {
            let (fs, _temp, root) = fixture(true);
            package_fixture(&fs, &root);
            fs.write(
                root.join("default.project.json"),
                r#"{"name":"Test","tree":{"$path":"src"}}"#,
            )
            .unwrap();
            let directory = root.join(".rojo/relayout/interrupted");
            fs.create_dir_all(&directory).unwrap();
            let original = root.join(if package_path {
                "src/Code.luau"
            } else {
                "dst/Code.luau"
            });
            fs.write(directory.join("original"), "before").unwrap();
            fs.write(
                directory.join("manifest.json"),
                serde_json::to_vec(&crate::syncback::relocation::RelayoutManifest {
                    entries: vec![(original.clone(), "original".into())],
                    package: None,
                })
                .unwrap(),
            )
            .unwrap();
            let before = fs.snapshot_tree(&root).unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            let result = crate::serve_session::ServeSession::new(restarted, &root);
            if package_path {
                assert!(result.is_err());
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            } else {
                assert!(result.is_ok());
                assert_eq!(std::fs::read(&original).unwrap(), b"before");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_refuses_relayout_and_cache_symlink_ancestors_without_mutation() {
        for linked_cache in [false, true] {
            let (fs, _temp, root) = fixture(true);
            fs.write(
                root.join("default.project.json"),
                r#"{"name":"Test","tree":{"$path":"src"}}"#,
            )
            .unwrap();
            let directory = root.join(".rojo/relayout/interrupted");
            fs.create_dir_all(&directory).unwrap();
            let original = root.join("dst/Code.luau");
            fs.write(directory.join("original"), "before").unwrap();
            fs.write(
                directory.join("manifest.json"),
                serde_json::to_vec(&crate::syncback::relocation::RelayoutManifest {
                    entries: vec![(original.clone(), "original".into())],
                    package: None,
                })
                .unwrap(),
            )
            .unwrap();
            let outside = tempfile::tempdir().unwrap();
            let linked = root.join(if linked_cache { ".rojo" } else { "dst" });
            let moved = outside.path().join("moved");
            std::fs::rename(&linked, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &linked).unwrap();
            let outside_before = fs.snapshot_tree(outside.path()).unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            assert!(crate::serve_session::ServeSession::new(restarted, &root).is_err());
            assert_eq!(fs.snapshot_tree(outside.path()).unwrap(), outside_before);
        }
    }

    #[test]
    fn startup_preserves_interrupted_package_batch_after_valid_generation_change() {
        let (fs, _temp, root) = fixture(true);
        package_fixture(&fs, &root);
        fs.write(
            root.join("default.project.json"),
            r#"{"name":"Test","tree":{"$path":"src"}}"#,
        )
        .unwrap();
        fs.create_dir_all(root.join(".rojo/relocations")).unwrap();
        let directory = root.join(".rojo/relocations/package-batch");
        let path = root.join("src/source.luau");
        fs.write(&path, "before").unwrap();
        execute_replacements(
            &fs,
            &directory,
            vec![Replacement {
                path: path.clone(),
                expected: Some(VfsSnapshot::file("before")),
                desired: Some(VfsSnapshot::file("after")),
            }],
        )
        .unwrap();
        fs.remove_file(directory.join("commit")).unwrap();
        let mut install: serde_json::Value =
            serde_json::from_slice(&fs.read(root.join("wally.install.json")).unwrap()).unwrap();
        install["generated_paths"] = serde_json::json!(["Packages/Changed.luau"]);
        fs.write(
            root.join("wally.install.json"),
            serde_json::to_vec(&install).unwrap(),
        )
        .unwrap();
        let restarted = Vfs::new(StdBackend::new().unwrap());
        restarted.set_watch_enabled(false);
        let policy = crate::package_sources::PackageSources::load(&restarted, &root)
            .unwrap()
            .unwrap();
        assert!(policy.problem.is_none());
        let backup = restarted.snapshot_tree(&directory).unwrap();
        let result = crate::serve_session::ServeSession::new(restarted, &root);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert_eq!(fs.snapshot_tree(&directory).unwrap(), backup);
    }

    #[cfg(unix)]
    #[test]
    fn package_recovery_refuses_replaced_symlink_ancestor_after_restart() {
        let (fs, _temp, root) = fixture(true);
        package_fixture(&fs, &root);
        fs.create_dir(root.join("src/Nested")).unwrap();
        let path = root.join("src/Nested/source.luau");
        let safe = root.join("src/safe.luau");
        let directory = root.join("cache/package-batch");
        fs.write(&path, "before").unwrap();
        fs.write(&safe, "safe before").unwrap();
        execute_replacements(
            &fs,
            &directory,
            vec![
                Replacement {
                    path: path.clone(),
                    expected: Some(VfsSnapshot::file("before")),
                    desired: Some(VfsSnapshot::file("after")),
                },
                Replacement {
                    path: safe.clone(),
                    expected: Some(VfsSnapshot::file("safe before")),
                    desired: Some(VfsSnapshot::file("safe after")),
                },
            ],
        )
        .unwrap();
        fs.remove_file(directory.join("commit")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::rename(root.join("src/Nested"), outside.path().join("Nested")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("Nested"), root.join("src/Nested")).unwrap();
        let restarted = Vfs::new(StdBackend::new().unwrap());
        restarted.set_watch_enabled(false);
        let backup = restarted.snapshot_tree(&directory).unwrap();
        assert!(recover(&restarted, &directory).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("Nested/source.luau")).unwrap(),
            b"after"
        );
        assert_eq!(std::fs::read(&safe).unwrap(), b"safe after");
        assert_eq!(restarted.snapshot_tree(&directory).unwrap(), backup);
    }

    #[test]
    fn package_replacement_restart_recovers_every_batch_boundary() {
        for real in [false, true] {
            for phase in 0..8 {
                let (fs, _temp, root) = fixture(real);
                package_fixture(&fs, &root);
                let entries = vec![
                    Replacement {
                        path: root.join("src/module.luau"),
                        expected: Some(VfsSnapshot::file("old source")),
                        desired: Some(VfsSnapshot::file("new source")),
                    },
                    Replacement {
                        path: root.join("src/Tree"),
                        expected: Some(VfsSnapshot::dir([(
                            "Before.luau",
                            VfsSnapshot::file("before"),
                        )])),
                        desired: Some(VfsSnapshot::dir([(
                            "After.luau",
                            VfsSnapshot::file("after"),
                        )])),
                    },
                    Replacement {
                        path: root.join("src/Added.luau"),
                        expected: None,
                        desired: Some(VfsSnapshot::file("added")),
                    },
                    Replacement {
                        path: root.join("src/Removed.luau"),
                        expected: Some(VfsSnapshot::file("removed")),
                        desired: None,
                    },
                ];
                let directory = root.join("cache/package-batch");
                let journal = ReplacementJournal {
                    version: JOURNAL_VERSION,
                    kind: "replacement".into(),
                    entries,
                    ownership: Some(ReplacementOwnership::capture(&fs, &directory).unwrap()),
                };
                {
                    let mut lock = fs.lock();
                    for entry in &journal.entries {
                        if let Some(expected) = &entry.expected {
                            materialize(&mut lock, &entry.path, expected).unwrap();
                        }
                    }
                    publish_intent(
                        &mut lock,
                        &directory,
                        &serde_json::to_vec(&journal).unwrap(),
                    )
                    .unwrap();
                    if phase >= 1 {
                        materialize(
                            &mut lock,
                            &directory.join("output-0"),
                            journal.entries[0].desired.as_ref().unwrap(),
                        )
                        .unwrap();
                    }
                    if phase >= 2 {
                        for (index, entry) in journal.entries.iter().enumerate().skip(1) {
                            if let Some(desired) = &entry.desired {
                                materialize(
                                    &mut lock,
                                    &directory.join(format!("output-{index}")),
                                    desired,
                                )
                                .unwrap();
                            }
                        }
                    }
                    if phase >= 3 {
                        transfer(
                            &mut lock,
                            &journal.entries[0].path,
                            &stage(&directory, 0),
                            journal.entries[0].expected.as_ref().unwrap(),
                        )
                        .unwrap();
                    }
                    if phase >= 4 {
                        for (index, entry) in journal.entries.iter().enumerate().skip(1) {
                            if let Some(expected) = &entry.expected {
                                transfer(
                                    &mut lock,
                                    &entry.path,
                                    &stage(&directory, index),
                                    expected,
                                )
                                .unwrap();
                            }
                        }
                    }
                    if phase >= 5 {
                        transfer(
                            &mut lock,
                            &directory.join("output-0"),
                            &journal.entries[0].path,
                            journal.entries[0].desired.as_ref().unwrap(),
                        )
                        .unwrap();
                    }
                    if phase >= 6 {
                        for (index, entry) in journal.entries.iter().enumerate().skip(1) {
                            if let Some(desired) = &entry.desired {
                                transfer(
                                    &mut lock,
                                    &directory.join(format!("output-{index}")),
                                    &entry.path,
                                    desired,
                                )
                                .unwrap();
                            }
                        }
                    }
                    if phase >= 7 {
                        persist(&mut lock, &directory, "commit", COMMIT).unwrap();
                    }
                }
                let restarted = if real {
                    Vfs::new(StdBackend::new().unwrap())
                } else {
                    fs
                };
                restarted.set_watch_enabled(false);
                let expected_result = if phase == 7 {
                    Recovery::Committed
                } else {
                    Recovery::RolledBack
                };
                assert_eq!(
                    recover(&restarted, &directory).unwrap(),
                    expected_result,
                    "phase {phase}, real {real}"
                );
                assert_eq!(recover(&restarted, &directory).unwrap(), expected_result);
                for entry in &journal.entries {
                    assert_eq!(
                        restarted
                            .snapshot_tree(&entry.path)
                            .with_not_found()
                            .unwrap(),
                        if phase == 7 {
                            entry.desired.clone()
                        } else {
                            entry.expected.clone()
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn package_replacement_validates_expected_content_before_publishing() {
        let (fs, _temp, root) = fixture(false);
        package_fixture(&fs, &root);
        let path = root.join("src/source.luau");
        fs.write(&path, "concurrent edit").unwrap();
        let directory = root.join("cache/package-batch");
        assert!(execute_replacements(
            &fs,
            &directory,
            vec![Replacement {
                path: path.clone(),
                expected: Some(VfsSnapshot::file("baseline")),
                desired: Some(VfsSnapshot::file("Studio edit"))
            }]
        )
        .is_err());
        assert_eq!(&*fs.read(&path).unwrap(), b"concurrent edit");
        assert!(fs.metadata(&directory).with_not_found().unwrap().is_none());
    }

    #[test]
    fn package_recovery_preserves_edited_sources_and_corrupt_backups() {
        for corrupt_backup in [false, true] {
            let (fs, _temp, root) = fixture(false);
            package_fixture(&fs, &root);
            let path = root.join("src/source.luau");
            let directory = root.join("cache/package-batch");
            fs.write(&path, "before").unwrap();
            execute_replacements(
                &fs,
                &directory,
                vec![Replacement {
                    path: path.clone(),
                    expected: Some(VfsSnapshot::file("before")),
                    desired: Some(VfsSnapshot::file("after")),
                }],
            )
            .unwrap();
            if corrupt_backup {
                fs.write(stage(&directory, 0), "edited backup").unwrap();
            } else {
                fs.remove_file(directory.join("commit")).unwrap();
                fs.write(&path, "concurrent source edit").unwrap();
            }
            let before = fs.snapshot_tree(&root).unwrap();
            assert!(recover(&fs, &directory).is_err());
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_replacements_preserve_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (fs, _temp, root) = fixture(true);
        package_fixture(&fs, &root);
        let path = root.join("src/source.luau");
        fs.write(&path, "before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        execute_replacements(
            &fs,
            &root.join("cache/package-batch"),
            vec![Replacement {
                path: path.clone(),
                expected: Some(VfsSnapshot::file("before")),
                desired: Some(VfsSnapshot::file("after")),
            }],
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    fn moves(fs: &Vfs, root: &Path) -> Vec<Relocation> {
        ["script.lua", "script.meta.json"]
            .into_iter()
            .map(|name| {
                let source = root.join("src").join(name);
                fs.write(&source, name).unwrap();
                Relocation {
                    expected: fs.snapshot_tree(&source).unwrap(),
                    source,
                    destination: root.join("dst").join(name),
                }
            })
            .collect()
    }

    fn intent(fs: &Vfs, directory: &Path, moves: &[Relocation]) {
        fs.create_dir(directory).unwrap();
        persist(
            &mut fs.lock(),
            directory,
            "intent.json",
            &serde_json::to_vec(&Journal {
                version: JOURNAL_VERSION,
                moves: moves.to_vec(),
                absent_paths: Vec::new(),
                ownership: None,
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn promotion_fixture(fs: &Vfs, root: &Path) -> Promotion {
        let directory = root.join("dst/Parent");
        fs.write(root.join("dst/Parent.server.lua"), "parent source")
            .unwrap();
        fs.write(root.join("dst/Parent.meta.jsonc"), "parent metadata")
            .unwrap();
        fs.create_dir(root.join("src/Child")).unwrap();
        fs.write(root.join("src/Child/init.luau"), "return 1")
            .unwrap();
        fs.write(root.join("src/Child/unmanaged.bin"), [0, 255])
            .unwrap();
        fs.write(root.join("src/Child/init.meta.json"), "child metadata")
            .unwrap();
        Promotion {
            destination: directory.clone(),
            originals: [
                ("dst/Parent.server.lua", "init.server.lua"),
                ("dst/Parent.meta.jsonc", "init.meta.jsonc"),
                ("src/Child", "Child"),
            ]
            .into_iter()
            .map(|(source, destination)| {
                let source = root.join(source);
                Relocation {
                    expected: fs.snapshot_tree(&source).unwrap(),
                    source,
                    destination: directory.join(destination),
                }
            })
            .collect(),
        }
    }

    fn promotion_intent(fs: &Vfs, directory: &Path, promotion: &Promotion) {
        fs.create_dir(directory).unwrap();
        persist(
            &mut fs.lock(),
            directory,
            "intent.json",
            &serde_json::to_vec(&PromotionJournal {
                version: JOURNAL_VERSION,
                kind: "promotion".into(),
                promotion: promotion.clone(),
                absent_paths: Vec::new(),
                ownership: None,
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn interrupted_preparations_do_not_block_startup_or_discard_unexpected_data() {
        for real in [false, true] {
            for state in [
                "empty",
                "partial",
                "move",
                "promotion",
                "unexpected",
                "edited",
            ] {
                let (fs, _temp, root) = fixture(real);
                let project_path = root.join("default.project.json");
                fs.write(
                    &project_path,
                    r#"{"name":"Test","tree":{"$className":"Folder"}}"#,
                )
                .unwrap();
                let project = crate::Project::load_exact(&fs, &project_path, Some("Test")).unwrap();
                let cache = super::super::relocation::cache_directory(&project);
                let preparation = cache.join(format!(".preparing-{}", uuid::Uuid::new_v4()));
                fs.create_dir_all(&preparation).unwrap();
                let originals = if state == "promotion" {
                    let plan = promotion_fixture(&fs, &root);
                    fs.write(
                        preparation.join("intent.json"),
                        serde_json::to_vec(&PromotionJournal {
                            version: JOURNAL_VERSION,
                            kind: "promotion".into(),
                            promotion: plan.clone(),
                            absent_paths: Vec::new(),
                            ownership: None,
                        })
                        .unwrap(),
                    )
                    .unwrap();
                    plan.originals
                } else {
                    let plan = moves(&fs, &root);
                    match state {
                        "move" => fs
                            .write(
                                preparation.join("intent.json"),
                                serde_json::to_vec(&Journal {
                                    version: JOURNAL_VERSION,
                                    moves: plan.clone(),
                                    absent_paths: Vec::new(),
                                    ownership: None,
                                })
                                .unwrap(),
                            )
                            .unwrap(),
                        "partial" => fs
                            .write(preparation.join("intent.json"), b"{\"version\":")
                            .unwrap(),
                        "unexpected" => fs
                            .write(preparation.join("stage-0"), "external data")
                            .unwrap(),
                        "edited" => {
                            let mut value = serde_json::to_value(&Journal {
                                version: JOURNAL_VERSION,
                                moves: plan.clone(),
                                absent_paths: Vec::new(),
                                ownership: None,
                            })
                            .unwrap();
                            value["external"] = "preserve this edit".into();
                            fs.write(
                                preparation.join("intent.json"),
                                serde_json::to_vec(&value).unwrap(),
                            )
                            .unwrap();
                        }
                        _ => {}
                    }
                    plan
                };
                let before = fs.snapshot_tree(&preparation).unwrap();
                super::super::relocation::recover_relocations(&fs, &project).unwrap();
                if matches!(state, "partial" | "unexpected" | "edited") {
                    assert_eq!(fs.snapshot_tree(&preparation).unwrap(), before);
                } else {
                    assert!(!fs.exists(&preparation).unwrap());
                }
                for entry in originals {
                    assert_eq!(fs.snapshot_tree(&entry.source).unwrap(), entry.expected);
                    assert!(!fs.exists(&entry.destination).unwrap());
                }
            }
        }
    }

    #[test]
    fn intent_publication_never_replaces_an_existing_journal() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let journal = root.join("cache/tx");
            fs.create_dir(&journal).unwrap();
            fs.write(journal.join("external"), "occupied").unwrap();
            let before = fs.snapshot_tree(&journal).unwrap();
            let bytes = serde_json::to_vec(&Journal {
                version: JOURNAL_VERSION,
                moves: plan.clone(),
                absent_paths: Vec::new(),
                ownership: None,
            })
            .unwrap();
            assert!(publish_intent(&mut fs.lock(), &journal, &bytes).is_err());
            assert_eq!(fs.snapshot_tree(&journal).unwrap(), before);
            let preparations: Vec<_> = fs
                .read_dir(root.join("cache"))
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.path() != journal)
                .collect();
            assert_eq!(preparations.len(), 1);
            assert!(preparation_is_discardable(&fs, preparations[0].path()).unwrap());
            for entry in plan {
                assert_eq!(fs.snapshot_tree(&entry.source).unwrap(), entry.expected);
                assert!(!fs.exists(&entry.destination).unwrap());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn promotion_preserves_unmanaged_executable_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (fs, _temp, root) = fixture(true);
        let plan = promotion_fixture(&fs, &root);
        std::fs::set_permissions(
            root.join("dst/Parent.server.lua"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("dst/Parent.meta.jsonc"),
            std::fs::Permissions::from_mode(0o440),
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("src/Child"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("src/Child/unmanaged.bin"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        execute_promotion(&fs, &root.join("cache/tx"), plan.clone(), Vec::new()).unwrap();
        for (name, expected) in [
            ("init.server.lua", 0o640),
            ("init.meta.jsonc", 0o440),
            ("Child", 0o751),
            ("Child/unmanaged.bin", 0o751),
        ] {
            assert_eq!(
                std::fs::metadata(plan.destination.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                expected
            );
        }
        assert_eq!(
            recover(&fs, &root.join("cache/tx")).unwrap(),
            Recovery::Committed
        );
    }

    #[test]
    fn permission_copies_in_memory_do_not_require_host_entries() {
        let (fs, _temp, root) = fixture(false);
        let source = root.join("src/script.lua");
        let destination = root.join("dst/script.lua");
        fs.write(&source, "original").unwrap();
        fs.write(&destination, "copy").unwrap();
        fs.copy_permissions(&source, &destination).unwrap();
        fs.lock().copy_permissions(&source, &destination).unwrap();
        assert_eq!(fs.read(&source).unwrap().as_slice(), b"original");
        assert_eq!(fs.read(&destination).unwrap().as_slice(), b"copy");
        assert!(fs
            .copy_permissions(&root.join("missing"), &destination)
            .is_err());
    }

    #[test]
    fn promotion_keeps_originals_until_commit_and_installs_exact_output() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = promotion_fixture(&fs, &root);
            let journal = root.join("cache/tx");
            execute_promotion(&fs, &journal, plan.clone(), Vec::new()).unwrap();
            assert_eq!(recover(&fs, &journal).unwrap(), Recovery::Committed);
            for (index, entry) in plan.originals.iter().enumerate() {
                assert!(!fs.exists(&entry.source).unwrap());
                assert_eq!(
                    fs.snapshot_tree(&entry.destination).unwrap(),
                    entry.expected
                );
                assert_eq!(
                    fs.snapshot_tree(&stage(&journal, index)).unwrap(),
                    entry.expected
                );
            }
        }
    }

    #[test]
    fn promotion_restart_recovers_every_materialize_stage_install_and_commit_boundary() {
        for real in [false, true] {
            // Intent, partial materialization, full materialization, each original
            // stage, output installation, reclamation, and durable commit.
            for boundary in 0..=8 {
                let (fs, _temp, root) = fixture(real);
                let plan = promotion_fixture(&fs, &root);
                let journal = root.join("cache/tx");
                let output = validate_promotion(&mut fs.lock(), &journal, &plan, true).unwrap();
                promotion_intent(&fs, &journal, &plan);
                if boundary == 1 {
                    fs.create_dir(journal.join("output")).unwrap();
                    fs.write(journal.join("output/init.server.lua"), "parent source")
                        .unwrap();
                } else if boundary >= 2 {
                    materialize(&mut fs.lock(), &journal.join("output"), &output).unwrap();
                }
                for (index, entry) in plan.originals.iter().enumerate() {
                    if boundary >= 3 + index {
                        fs.move_no_replace(&entry.source, &stage(&journal, index), &entry.expected)
                            .unwrap();
                    }
                }
                if boundary >= 6 {
                    fs.move_no_replace(&journal.join("output"), &plan.destination, &output)
                        .unwrap();
                }
                if boundary == 7 {
                    fs.move_no_replace(&plan.destination, &journal.join("reclaimed"), &output)
                        .unwrap();
                }
                if boundary == 8 {
                    persist(&mut fs.lock(), &journal, "commit", COMMIT).unwrap();
                }
                // The real backend is reopened to avoid relying on Vfs session state.
                let reopened = real.then(|| Vfs::new(StdBackend::new().unwrap()));
                let fs = reopened.as_ref().unwrap_or(&fs);
                fs.set_watch_enabled(false);
                for _ in 0..2 {
                    assert_eq!(
                        recover(fs, &journal).unwrap(),
                        if boundary == 8 {
                            Recovery::Committed
                        } else {
                            Recovery::RolledBack
                        }
                    );
                }
                for entry in &plan.originals {
                    if boundary == 8 {
                        assert_eq!(
                            fs.snapshot_tree(&entry.destination).unwrap(),
                            entry.expected
                        );
                    } else {
                        assert_eq!(fs.snapshot_tree(&entry.source).unwrap(), entry.expected);
                        assert!(!fs.exists(&plan.destination).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn promotion_preserves_ambiguous_originals_stages_and_installed_edits() {
        for real in [false, true] {
            for edit in ["source", "stage", "installed", "private", "marker"] {
                let (fs, _temp, root) = fixture(real);
                let plan = promotion_fixture(&fs, &root);
                let journal = root.join("cache/tx");
                let output = validate_promotion(&mut fs.lock(), &journal, &plan, true).unwrap();
                promotion_intent(&fs, &journal, &plan);
                materialize(&mut fs.lock(), &journal.join("output"), &output).unwrap();
                for (index, entry) in plan.originals.iter().enumerate() {
                    fs.move_no_replace(&entry.source, &stage(&journal, index), &entry.expected)
                        .unwrap();
                }
                fs.move_no_replace(&journal.join("output"), &plan.destination, &output)
                    .unwrap();
                let edited = match edit {
                    "source" => plan.originals[0].source.clone(),
                    "stage" => stage(&journal, 0),
                    "installed" => plan.originals[0].destination.clone(),
                    "private" => journal.join("output"),
                    _ => journal.join("commit"),
                };
                fs.write(&edited, "editor bytes").unwrap();
                assert!(recover(&fs, &journal).is_err(), "{edit}");
                assert_eq!(fs.read(&edited).unwrap().as_slice(), b"editor bytes");
                assert!(!fs.exists(journal.join("rollback")).unwrap());
                assert!(fs.exists(journal.join("intent.json")).unwrap());
            }
        }
    }

    #[test]
    fn promotion_validates_both_markers_and_preserves_edited_committed_originals() {
        for real in [false, true] {
            for (commit, rollback) in [
                (COMMIT, ROLLBACK),
                (COMMIT, b"invalid".as_slice()),
                (b"invalid".as_slice(), ROLLBACK),
            ] {
                let (fs, _temp, root) = fixture(real);
                let plan = promotion_fixture(&fs, &root);
                let journal = root.join("cache/tx");
                execute_promotion(&fs, &journal, plan, Vec::new()).unwrap();
                fs.write(journal.join("commit"), commit).unwrap();
                fs.write(journal.join("rollback"), rollback).unwrap();
                let before = fs.snapshot_tree(&root).unwrap();
                assert_eq!(
                    recover(&fs, &journal).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            }
            let (fs, _temp, root) = fixture(real);
            let plan = promotion_fixture(&fs, &root);
            let journal = root.join("cache/tx");
            execute_promotion(&fs, &journal, plan.clone(), Vec::new()).unwrap();
            // Edits to committed output are normal subsequent work, not rollback input.
            fs.write(&plan.originals[0].destination, "later source edit")
                .unwrap();
            assert_eq!(recover(&fs, &journal).unwrap(), Recovery::Committed);
            fs.write(stage(&journal, 0), "edited saved original")
                .unwrap();
            let before = fs.snapshot_tree(&root).unwrap();
            assert!(recover(&fs, &journal).is_err());
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[test]
    fn promotion_faults_during_stage_install_and_commit_fail_closed_and_resume() {
        for real in [false, true] {
            for failure in ["stage", "install", "commit"] {
                let (fs, _temp, root) = fixture(real);
                let plan = promotion_fixture(&fs, &root);
                let journal = root.join("cache/tx");
                let output = validate_promotion(&mut fs.lock(), &journal, &plan, true).unwrap();
                promotion_intent(&fs, &journal, &plan);
                let blocker = match failure {
                    "stage" => stage(&journal, 1),
                    "install" => plan.destination.clone(),
                    _ => journal.join("commit"),
                };
                fs.write(&blocker, "intervening edit").unwrap();
                assert!(
                    execute_promotion_prepared(&mut fs.lock(), &journal, &plan, &output, &[])
                        .is_err()
                );
                assert_eq!(fs.read(&blocker).unwrap().as_slice(), b"intervening edit");
                fs.remove_file(&blocker).unwrap();
                assert_eq!(recover(&fs, &journal).unwrap(), Recovery::RolledBack);
                for entry in plan.originals {
                    assert_eq!(fs.snapshot_tree(&entry.source).unwrap(), entry.expected);
                }
            }
        }
    }

    #[test]
    fn promotion_preflight_rejects_collisions_stale_sources_and_nested_originals() {
        for real in [false, true] {
            for failure in ["output", "duplicate", "stale", "nested"] {
                let (fs, _temp, root) = fixture(real);
                let mut plan = promotion_fixture(&fs, &root);
                let journal = root.join("cache/tx");
                match failure {
                    "output" => fs.create_dir(&plan.destination).unwrap(),
                    "duplicate" => {
                        plan.originals[1].destination = plan.originals[0].destination.clone()
                    }
                    "stale" => fs.write(&plan.originals[0].source, "new source").unwrap(),
                    _ => plan.originals[1].source = plan.originals[2].source.join("init.luau"),
                }
                let before = fs.snapshot_tree(&root).unwrap();
                assert!(execute_promotion(&fs, &journal, plan, Vec::new()).is_err());
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            }
        }
    }

    #[test]
    fn commits_whole_directories_and_sidecars_on_both_backends() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let mut plan = moves(&fs, &root);
            let source = root.join("src/folder");
            fs.create_dir(&source).unwrap();
            fs.write(source.join("unmanaged.bin"), [0, 255]).unwrap();
            plan.push(Relocation {
                source: source.clone(),
                destination: root.join("dst/folder"),
                expected: fs.snapshot_tree(&source).unwrap(),
            });
            let directory = root.join("cache/tx");
            execute(&fs, &directory, plan.clone(), Vec::new()).unwrap();
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
            for relocation in plan {
                assert!(!fs.exists(&relocation.source).unwrap());
                assert_eq!(
                    fs.snapshot_tree(&relocation.destination).unwrap(),
                    relocation.expected
                );
            }
        }
    }

    #[test]
    fn contradictory_or_corrupt_terminal_markers_fail_without_modifying_data() {
        for real in [false, true] {
            for (commit, rollback) in [
                (COMMIT, ROLLBACK),
                (COMMIT, b"invalid".as_slice()),
                (b"invalid".as_slice(), ROLLBACK),
            ] {
                let (fs, _temp, root) = fixture(real);
                let plan = moves(&fs, &root);
                let directory = root.join("cache/tx");
                execute(&fs, &directory, plan.clone(), Vec::new()).unwrap();
                fs.write(directory.join("commit"), commit).unwrap();
                fs.write(directory.join("rollback"), rollback).unwrap();
                let before = fs.snapshot_tree(&root).unwrap();
                let error = recover(&fs, &directory).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            }
        }
    }

    #[test]
    fn recovers_every_interrupted_move_boundary_idempotently() {
        for real in [false, true] {
            for boundary in 0..=4 {
                let (fs, _temp, root) = fixture(real);
                let plan = moves(&fs, &root);
                let directory = root.join("cache/tx");
                intent(&fs, &directory, &plan);
                for (index, relocation) in plan.iter().enumerate() {
                    if boundary > index {
                        fs.move_no_replace(
                            &relocation.source,
                            &stage(&directory, index),
                            &relocation.expected,
                        )
                        .unwrap();
                    }
                }
                for (index, relocation) in plan.iter().enumerate() {
                    if boundary > index + 2 {
                        fs.move_no_replace(
                            &stage(&directory, index),
                            &relocation.destination,
                            &relocation.expected,
                        )
                        .unwrap();
                    }
                }
                for _ in 0..2 {
                    assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
                }
                for relocation in plan {
                    assert_eq!(
                        fs.snapshot_tree(&relocation.source).unwrap(),
                        relocation.expected
                    );
                    assert!(!fs.exists(&relocation.destination).unwrap());
                }
            }
        }
    }

    #[test]
    fn sidecar_collision_rolls_back_without_touching_intervening_destination() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let directory = root.join("cache/tx");
            intent(&fs, &directory, &plan);
            // Simulate an editor creation after preflight and durable intent. This
            // drives the same failure/automatic-rollback path as execute, after the
            // primary file has been installed but the sidecar cannot be installed.
            fs.write(&plan[1].destination, "editor").unwrap();
            assert!(execute_prepared(
                &mut fs.lock(),
                &directory,
                &Journal {
                    version: JOURNAL_VERSION,
                    moves: plan.clone(),
                    absent_paths: Vec::new(),
                    ownership: None,
                }
            )
            .is_err());
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
            assert_eq!(fs.read(&plan[1].destination).unwrap().as_slice(), b"editor");
            for relocation in plan {
                assert_eq!(
                    fs.snapshot_tree(&relocation.source).unwrap(),
                    relocation.expected
                );
            }
        }
    }

    #[test]
    fn ambiguous_or_edited_staging_is_retained_for_manual_resolution() {
        for real in [false, true] {
            for edit_staged in [false, true] {
                let (fs, _temp, root) = fixture(real);
                let plan = moves(&fs, &root);
                let directory = root.join("cache/tx");
                intent(&fs, &directory, &plan);
                fs.move_no_replace(&plan[0].source, &stage(&directory, 0), &plan[0].expected)
                    .unwrap();
                let edited = if edit_staged {
                    stage(&directory, 0)
                } else {
                    plan[0].source.clone()
                };
                fs.write(&edited, "editor").unwrap();
                assert!(recover(&fs, &directory).is_err());
                assert_eq!(fs.read(&edited).unwrap().as_slice(), b"editor");
                assert!(fs.exists(stage(&directory, 0)).unwrap());
                assert!(fs.exists(directory.join("intent.json")).unwrap());
                assert!(!fs.exists(directory.join("rollback")).unwrap());
            }
        }
    }

    #[test]
    fn identical_recreated_original_is_still_ambiguous() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let directory = root.join("cache/tx");
            intent(&fs, &directory, &plan);
            fs.move_no_replace(&plan[0].source, &plan[0].destination, &plan[0].expected)
                .unwrap();
            fs.write(&plan[0].source, "script.lua").unwrap();
            assert!(recover(&fs, &directory).is_err());
            assert_eq!(fs.snapshot_tree(&plan[0].source).unwrap(), plan[0].expected);
            assert_eq!(
                fs.snapshot_tree(&plan[0].destination).unwrap(),
                plan[0].expected
            );
            assert!(!fs.exists(directory.join("rollback")).unwrap());
        }
    }

    #[test]
    fn resumes_partial_rollback_after_manual_conflict_resolution() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let directory = root.join("cache/tx");
            intent(&fs, &directory, &plan);
            for relocation in &plan {
                fs.move_no_replace(
                    &relocation.source,
                    &relocation.destination,
                    &relocation.expected,
                )
                .unwrap();
            }
            fs.write(&plan[0].source, "editor").unwrap();
            assert!(recover(&fs, &directory).is_err());
            // Recovery restores independent entries even when one needs attention.
            assert_eq!(fs.snapshot_tree(&plan[1].source).unwrap(), plan[1].expected);
            assert!(!fs.exists(&plan[1].destination).unwrap());
            let saved_edit = root.join("saved-editor-file");
            fs.move_no_replace(
                &plan[0].source,
                &saved_edit,
                &VfsSnapshot::file(b"editor".to_vec()),
            )
            .unwrap();
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
            assert_eq!(fs.read(&saved_edit).unwrap().as_slice(), b"editor");
            for relocation in plan {
                assert_eq!(
                    fs.snapshot_tree(&relocation.source).unwrap(),
                    relocation.expected
                );
                assert!(!fs.exists(&relocation.destination).unwrap());
            }
        }
    }

    #[test]
    fn edited_installed_files_and_corrupt_journals_fail_closed() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let directory = root.join("cache/tx");
            intent(&fs, &directory, &plan);
            fs.move_no_replace(&plan[0].source, &plan[0].destination, &plan[0].expected)
                .unwrap();
            fs.write(&plan[0].destination, "editor").unwrap();
            assert!(recover(&fs, &directory).is_err());
            assert_eq!(fs.read(&plan[0].destination).unwrap().as_slice(), b"editor");
            assert!(!fs.exists(&plan[0].source).unwrap());
            fs.write(directory.join("commit"), "invalid marker")
                .unwrap();
            assert_eq!(
                recover(&fs, &directory).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            fs.remove_file(directory.join("commit")).unwrap();
            fs.write(directory.join("intent.json"), "invalid json")
                .unwrap();
            assert!(recover(&fs, &directory).is_err());
            assert_eq!(fs.read(&plan[0].destination).unwrap().as_slice(), b"editor");
        }
    }

    #[test]
    fn absent_move_sidecars_are_guarded_before_staging_and_commit() {
        for real in [false, true] {
            for destination in [false, true] {
                for installed in [false, true] {
                    let (fs, _temp, root) = fixture(real);
                    let plan = moves(&fs, &root);
                    let directory = root.join("cache/tx");
                    let absent = root.join(if destination {
                        "dst/script.meta.jsonc"
                    } else {
                        "src/script.meta.jsonc"
                    });
                    let journal = Journal {
                        version: JOURNAL_VERSION,
                        moves: plan.clone(),
                        absent_paths: vec![absent.clone()],
                        ownership: None,
                    };
                    fs.create_dir(&directory).unwrap();
                    persist(
                        &mut fs.lock(),
                        &directory,
                        "intent.json",
                        &serde_json::to_vec(&journal).unwrap(),
                    )
                    .unwrap();
                    let saved: Journal =
                        serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap())
                            .unwrap();
                    assert_eq!(saved.absent_paths, vec![absent.clone()]);
                    if installed {
                        for entry in &plan {
                            transfer(
                                &mut fs.lock(),
                                &entry.source,
                                &entry.destination,
                                &entry.expected,
                            )
                            .unwrap();
                        }
                    }
                    fs.write(&absent, "external metadata").unwrap();
                    if installed {
                        assert!(commit(
                            &mut fs.lock(),
                            &directory,
                            &saved.absent_paths,
                            &saved.moves
                        )
                        .is_err());
                    } else {
                        assert!(execute_prepared(&mut fs.lock(), &directory, &saved).is_err());
                    }
                    assert!(!fs.exists(directory.join("commit")).unwrap());
                    assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
                    assert_eq!(fs.read(&absent).unwrap().as_slice(), b"external metadata");
                    for entry in &plan {
                        assert_eq!(fs.snapshot_tree(&entry.source).unwrap(), entry.expected);
                        assert!(!fs.exists(&entry.destination).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn absent_promotion_sidecars_preserve_external_creations() {
        for real in [false, true] {
            for destination in [false, true] {
                for installed in [false, true] {
                    let (fs, _temp, root) = fixture(real);
                    let plan = promotion_fixture(&fs, &root);
                    let directory = root.join("cache/tx");
                    let absent = if destination {
                        plan.destination.join("init.meta.json")
                    } else {
                        root.join("dst/Parent.meta.json")
                    };
                    let journal = PromotionJournal {
                        version: JOURNAL_VERSION,
                        kind: "promotion".into(),
                        promotion: plan.clone(),
                        absent_paths: vec![absent.clone()],
                        ownership: None,
                    };
                    fs.create_dir(&directory).unwrap();
                    persist(
                        &mut fs.lock(),
                        &directory,
                        "intent.json",
                        &serde_json::to_vec(&journal).unwrap(),
                    )
                    .unwrap();
                    let saved: PromotionJournal =
                        serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap())
                            .unwrap();
                    assert_eq!(saved.absent_paths, vec![absent.clone()]);
                    let output =
                        validate_promotion(&mut fs.lock(), &directory, &plan, true).unwrap();
                    if installed {
                        materialize(&mut fs.lock(), &plan.destination, &output).unwrap();
                        for (index, entry) in plan.originals.iter().enumerate() {
                            transfer(
                                &mut fs.lock(),
                                &entry.source,
                                &stage(&directory, index),
                                &entry.expected,
                            )
                            .unwrap();
                        }
                    } else if destination {
                        fs.create_dir(&plan.destination).unwrap();
                    }
                    fs.write(&absent, "external metadata").unwrap();
                    if installed {
                        assert!(commit(
                            &mut fs.lock(),
                            &directory,
                            &saved.absent_paths,
                            &saved.promotion.originals
                        )
                        .is_err());
                    } else {
                        assert!(execute_promotion_prepared(
                            &mut fs.lock(),
                            &directory,
                            &plan,
                            &output,
                            &saved.absent_paths
                        )
                        .is_err());
                    }
                    assert!(!fs.exists(directory.join("commit")).unwrap());
                    if destination {
                        // Modified output is ambiguous. Preserve it and every original
                        // rather than removing the editor's metadata during rollback.
                        assert!(recover(&fs, &directory).is_err());
                    } else {
                        assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
                    }
                    assert_eq!(fs.read(&absent).unwrap().as_slice(), b"external metadata");
                    for (index, entry) in plan.originals.iter().enumerate() {
                        let path = if installed && destination {
                            stage(&directory, index)
                        } else {
                            entry.source.clone()
                        };
                        assert_eq!(fs.snapshot_tree(&path).unwrap(), entry.expected);
                    }
                }
            }
        }
    }

    #[test]
    fn recreated_existing_sidecars_prevent_commit_without_discarding_either_version() {
        for real in [false, true] {
            for promotion in [false, true] {
                let (fs, _temp, root) = fixture(real);
                let directory = root.join("cache/tx");
                let originals = if promotion {
                    let plan = promotion_fixture(&fs, &root);
                    promotion_intent(&fs, &directory, &plan);
                    let output =
                        validate_promotion(&mut fs.lock(), &directory, &plan, true).unwrap();
                    materialize(&mut fs.lock(), &directory.join("output"), &output).unwrap();
                    for (index, entry) in plan.originals.iter().enumerate() {
                        transfer(
                            &mut fs.lock(),
                            &entry.source,
                            &stage(&directory, index),
                            &entry.expected,
                        )
                        .unwrap();
                    }
                    transfer(
                        &mut fs.lock(),
                        &directory.join("output"),
                        &plan.destination,
                        &output,
                    )
                    .unwrap();
                    plan.originals
                } else {
                    let plan = moves(&fs, &root);
                    intent(&fs, &directory, &plan);
                    for (index, entry) in plan.iter().enumerate() {
                        transfer(
                            &mut fs.lock(),
                            &entry.source,
                            &stage(&directory, index),
                            &entry.expected,
                        )
                        .unwrap();
                        transfer(
                            &mut fs.lock(),
                            &stage(&directory, index),
                            &entry.destination,
                            &entry.expected,
                        )
                        .unwrap();
                    }
                    plan
                };
                let sidecar = &originals[1];
                fs.write(&sidecar.source, "recreated metadata").unwrap();
                assert!(commit(&mut fs.lock(), &directory, &[], &originals).is_err());
                assert!(!fs.exists(directory.join("commit")).unwrap());
                assert!(recover(&fs, &directory).is_err());
                assert_eq!(
                    fs.read(&sidecar.source).unwrap().as_slice(),
                    b"recreated metadata"
                );
                let preserved = if promotion {
                    stage(&directory, 1)
                } else {
                    sidecar.destination.clone()
                };
                assert_eq!(fs.snapshot_tree(&preserved).unwrap(), sidecar.expected);
                assert!(fs.exists(directory.join("intent.json")).unwrap());
            }
        }
    }

    #[test]
    fn committed_absence_guards_allow_later_metadata_edits() {
        for real in [false, true] {
            for promotion in [false, true] {
                let (fs, _temp, root) = fixture(real);
                let directory = root.join("cache/tx");
                let absent = root.join("src/later.meta.json");
                if promotion {
                    let plan = promotion_fixture(&fs, &root);
                    execute_promotion(&fs, &directory, plan, vec![absent.clone()]).unwrap();
                } else {
                    let plan = moves(&fs, &root);
                    execute(&fs, &directory, plan, vec![absent.clone()]).unwrap();
                }
                fs.write(&absent, "later edit").unwrap();
                assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
                assert_eq!(fs.read(&absent).unwrap().as_slice(), b"later edit");
            }
        }
    }

    #[test]
    fn expected_directory_includes_unmanaged_files() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            fs.write(root.join("src/unmanaged"), "original").unwrap();
            let source = root.join("src");
            let expected = fs.snapshot_tree(&source).unwrap();
            fs.write(source.join("unmanaged"), "editor").unwrap();
            assert!(execute(
                &fs,
                &root.join("cache/tx"),
                vec![Relocation {
                    source: source.clone(),
                    destination: root.join("new"),
                    expected,
                }],
                Vec::new(),
            )
            .is_err());
            assert_eq!(
                fs.read(source.join("unmanaged")).unwrap().as_slice(),
                b"editor"
            );
            assert!(!fs.exists(root.join("cache/tx")).unwrap());
        }
    }

    #[test]
    fn rejects_stale_sources_existing_destinations_and_overlapping_plans() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            let plan = moves(&fs, &root);
            let directory = root.join("cache/tx");
            fs.write(&plan[1].source, "changed").unwrap();
            assert!(execute(&fs, &directory, plan.clone(), Vec::new()).is_err());
            assert!(!fs.exists(&directory).unwrap());
            fs.write(&plan[1].source, "script.meta.json").unwrap();
            fs.write(&plan[1].destination, "editor").unwrap();
            assert!(execute(&fs, &directory, plan.clone(), Vec::new()).is_err());
            let mut overlapping = plan.clone();
            overlapping[1].destination = plan[0].source.clone();
            assert!(execute(&fs, &directory, overlapping, Vec::new()).is_err());
            assert_eq!(fs.snapshot_tree(&plan[0].source).unwrap(), plan[0].expected);
        }
    }
}
