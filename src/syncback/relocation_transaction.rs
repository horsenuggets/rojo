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
/// Whether rolling back this journal would write outside its project folder. Only the
/// process that wrote such a journal can vouch for that authority, so startup recovery
/// refuses these instead of trusting a journal that a checkout may have shipped.
/// Finished journals only clean up their own directory and are never refused.
pub(crate) fn rolls_back_outside_project(vfs: &Vfs, journal_dir: &Path) -> io::Result<bool> {
    let bytes = read_file(&vfs.lock(), &journal_dir.join("intent.json"))?;
    let intent: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if intent.get("kind").and_then(serde_json::Value::as_str) != Some("replacement") {
        return Ok(false);
    }
    let journal: ReplacementJournal = serde_json::from_value(intent).map_err(io::Error::other)?;
    let Some(root) = journal
        .script
        .as_ref()
        .and_then(|script| script.project.parent())
    else {
        return Ok(false);
    };
    let finished = ["commit", "rollback"]
        .iter()
        .any(|name| vfs.metadata(journal_dir.join(name)).is_ok());
    Ok(!finished
        && journal
            .entries
            .iter()
            .any(|entry| !entry.path.starts_with(root)))
}

pub fn recover(vfs: &Vfs, journal_dir: &Path) -> io::Result<Recovery> {
    // Validate current authority before acquiring the VFS lock. The locked recovery
    // checks the same durable evidence again before performing any mutation.
    require_canonical_parent(&mut vfs.lock(), &journal_dir.join("intent.json"))?;
    let bytes = read_file(&vfs.lock(), &journal_dir.join("intent.json"))?;
    let intent: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if intent.get("kind").and_then(serde_json::Value::as_str) == Some("replacement") {
        let journal: ReplacementJournal =
            serde_json::from_value(intent).map_err(io::Error::other)?;
        if let Some(ownership) = &journal.ownership {
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
        } else if journal.script.is_none() {
            return Err(recovery_refused(
                journal_dir,
                "Legacy package journal has no durable ownership evidence",
            ));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    script: Option<ScriptReplacementEvidence>,
}

/// Distinguishes narrowly scoped application script writes from legacy package
/// journals. Missing package evidence alone must never authorize recovery.
#[derive(Debug, Serialize, Deserialize)]
struct ScriptReplacementEvidence {
    project: PathBuf,
    expected: VfsSnapshot,
    source: PathBuf,
    destination: PathBuf,
    // Required even for old script journals. The root witness cannot reconstruct
    // historical nested declarations or absent probes after a restart.
    project_guards: Vec<(PathBuf, Option<Vec<u8>>)>,
    #[serde(default)]
    migration_ownership: Option<super::ScriptMigrationOwnership>,
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

pub(crate) fn same_canonical_spelling(path: &Path, canonical: &Path) -> bool {
    if path == canonical {
        return true;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut path = path.components();
        let mut canonical = canonical.components();
        let equivalent_prefix = match (path.next(), canonical.next()) {
            (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => {
                match (a.kind(), b.kind()) {
                    (Prefix::Disk(a), Prefix::VerbatimDisk(b))
                    | (Prefix::VerbatimDisk(a), Prefix::Disk(b)) => a == b,
                    (Prefix::UNC(a, b), Prefix::VerbatimUNC(c, d))
                    | (Prefix::VerbatimUNC(a, b), Prefix::UNC(c, d)) => a == c && b == d,
                    _ => false,
                }
            }
            _ => false,
        };
        // Canonicalization keeps the extended prefix for long Windows paths. Only
        // that namespace spelling may differ; ancestry and directory names may not.
        equivalent_prefix
            && path.clone().eq(canonical)
            && path.all(|component| match component {
                Component::RootDir => true,
                Component::Normal(name) => {
                    // Verbatim paths keep reserved names and trailing dots/spaces
                    // literal. Check each short component using dunce's Windows rules,
                    // without rejecting an otherwise valid long overall path.
                    let probe = Path::new(r"\\?\C:\").join(name);
                    dunce::simplified(&probe) != probe
                }
                _ => false,
            })
    }
    #[cfg(not(windows))]
    false
}

pub(crate) fn require_canonical_parent(fs: &mut VfsLock<'_>, path: &Path) -> io::Result<()> {
    let result = (|| {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Path has no parent"))?;
        if !same_canonical_spelling(parent, &fs.normalize(parent)?)
            || !fs.metadata(parent)?.is_dir()
        {
            return Err(io::Error::other(
                "Parent is not an existing canonical directory",
            ));
        }
        Ok(())
    })();
    result.map_err(|error| io::Error::new(error.kind(), format!("Recovery cannot validate the parent of {} ({error}). Restore the original directories before retrying; journal and backups are retained", path.display())))
}

fn validate_script_project_guards(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    journal: &ReplacementJournal,
    script: &ScriptReplacementEvidence,
) -> io::Result<()> {
    let VfsSnapshot::File { contents } = &script.expected else {
        return Err(conflict(&script.project));
    };
    if !script
        .project_guards
        .iter()
        .any(|(path, expected)| path == &script.project && expected.as_ref() == Some(contents))
    {
        return Err(conflict(&script.project));
    }
    // Guards may describe referenced projects outside the served root, but are
    // read-only and must never overlap write targets or private journal data.
    let mut paths = vec![directory];
    paths.extend(journal.entries.iter().map(|entry| entry.path.as_path()));
    validate_paths(fs, &paths, false)?;
    paths.extend(script.project_guards.iter().map(|(path, _)| path.as_path()));
    validate_path_layout(fs, &paths)?;
    if let Some(ownership) = &script.migration_ownership {
        let writes: Vec<_> = journal
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        ownership.validate(fs, &writes)?;
        // Layout validation above rejects duplicates before indexing. Borrow both the
        // paths and frozen bytes; an absent declaration is distinct from a missing guard.
        let guards: std::collections::HashMap<_, _> = script
            .project_guards
            .iter()
            .map(|(path, bytes)| {
                #[cfg(test)]
                tests::DECLARATION_PROBES.with(|count| count.set(count.get() + 1));
                (path.as_path(), bytes)
            })
            .collect();
        for (path, bytes) in &ownership.declarations {
            #[cfg(test)]
            tests::DECLARATION_PROBES.with(|count| count.set(count.get() + 1));
            if guards.get(path.as_path()).copied() != Some(bytes) {
                return Err(conflict(path));
            }
        }
    }
    for (path, expected) in &script.project_guards {
        // Windows accepts both separators, so raw OS-string equality would reject
        // ordinary joins. Reject explicit dot segments without changing that spelling.
        if (!crate::Project::is_project_file(path)
            && !script
                .migration_ownership
                .as_ref()
                .is_some_and(|ownership| ownership.declarations.contains_key(path)))
            || path
                .as_os_str()
                .as_encoded_bytes()
                .split(|byte| std::path::is_separator(char::from(*byte)))
                .any(|component| component == b".")
        {
            return Err(conflict(path));
        }
        if let Some(metadata) = fs.metadata(path).with_not_found()? {
            require_canonical_parent(fs, path)?;
            if !metadata.is_file() || !same_canonical_spelling(path, &fs.normalize(path)?) {
                return Err(conflict(path));
            }
        } else {
            // A read-only optional declaration need not have an existing parent.
            // Reuse its frozen identity proof, never infer new recovery authority
            // from today's absence. Legacy evidence still needs a canonical parent.
            if let Some(identity) = script
                .migration_ownership
                .as_ref()
                .and_then(|ownership| ownership.paths.get(path))
            {
                if !same_canonical_spelling(path, identity) {
                    return Err(conflict(path));
                }
            } else {
                require_canonical_parent(fs, path)?;
            }
            let canonical =
                super::canonical_dependency_path(fs, path).ok_or_else(|| conflict(path))?;
            if !same_canonical_spelling(path, &canonical) {
                return Err(conflict(path));
            }
        }
        if fs
            .read(path)
            .with_not_found()?
            .as_deref()
            .map(Vec::as_slice)
            != expected.as_deref()
        {
            return Err(conflict(path));
        }
    }
    Ok(())
}

fn validate_replacement_ownership(
    fs: &mut VfsLock<'_>,
    directory: &Path,
    journal: &ReplacementJournal,
) -> io::Result<()> {
    if let Some(script) = &journal.script {
        let root = script
            .project
            .parent()
            .ok_or_else(|| conflict(&script.project))?;
        validate_script_project_guards(fs, directory, journal, script)?;
        if directory.parent() != Some(root.join(".rojo/relocations").as_path()) {
            return Err(conflict(&script.project));
        }
        let source = journal
            .entries
            .iter()
            .find(|entry| entry.path == script.source);
        let destination = journal
            .entries
            .iter()
            .find(|entry| entry.path == script.destination);
        if script.source == script.destination
            || script.source.parent() != script.destination.parent()
            || script.source.extension() != script.destination.extension()
            || !script
                .source
                .extension()
                .is_some_and(|ext| ext == "lua" || ext == "luau")
            || !source.is_some_and(|entry| entry.expected.is_some() && entry.desired.is_none())
            || !destination.is_some_and(|entry| entry.expected.is_none() && entry.desired.is_some())
        {
            return Err(conflict(&script.source));
        }
        for entry in &journal.entries {
            let sidecar = entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".meta.json") || name.ends_with(".meta.jsonc"));
            // A declaration identity is read authority, not write authority. New
            // journals bind every write to the scoped serializer and its exact source
            // pair; older evidence remains confined to the project directory.
            let authority = script.migration_ownership.as_ref().and_then(|ownership| {
                ownership.write_authority.as_ref().map(|authority| {
                    authority.source == script.source
                        && authority.destination == script.destination
                        && authority.paths.contains(&entry.path)
                        && ownership.paths.get(&entry.path).is_some_and(|canonical| {
                            same_canonical_spelling(&entry.path, canonical)
                        })
                })
            });
            if authority == Some(false)
                || (!entry.path.starts_with(root) && authority != Some(true))
                || entry.path.parent() != script.source.parent()
                || (entry.path != script.source && entry.path != script.destination && !sidecar)
                || entry.path.starts_with(root.join(".rojo"))
                || entry.path == script.project
                || [&entry.expected, &entry.desired]
                    .into_iter()
                    .flatten()
                    .any(|snapshot| !matches!(snapshot, VfsSnapshot::File { .. }))
            {
                return Err(conflict(&entry.path));
            }
        }
        if journal.ownership.is_none() {
            // External mappings introduce source ancestors that are not ancestors
            // of the cache. They must not acquire unproven package ownership either.
            for ancestor in directory.ancestors().skip(1).chain(
                journal
                    .entries
                    .iter()
                    .filter(|entry| !entry.path.starts_with(root))
                    .flat_map(|entry| entry.path.ancestors().skip(1)),
            ) {
                if fs
                    .metadata(ancestor.join("wally.toml"))
                    .with_not_found()?
                    .is_some()
                {
                    return Err(recovery_refused(
                        directory,
                        "Package ownership appeared during script migration",
                    ));
                }
            }
            return Ok(());
        }
    }
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

fn validate_replacements(
    fs: &VfsLock<'_>,
    directory: &Path,
    journal: &ReplacementJournal,
) -> io::Result<()> {
    if journal.version != JOURNAL_VERSION || journal.kind != "replacement" {
        return Err(io::Error::other("Unknown replacement journal"));
    }
    for (index, entry) in journal.entries.iter().enumerate() {
        if !fs.is_absolute_path(&entry.path)
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
        script: None,
    };
    execute_replacement_journal(vfs, directory, journal, &Default::default())
}

#[cfg(test)]
fn execute_script_replacements(
    vfs: &Vfs,
    directory: &Path,
    project: &crate::Project,
    entries: Vec<Replacement>,
    expected: &std::collections::HashMap<PathBuf, Option<Vec<u8>>>,
) -> io::Result<()> {
    execute_script_replacements_guarded(vfs, directory, project, entries, expected, None)
}

pub(crate) fn execute_script_replacements_guarded(
    vfs: &Vfs,
    directory: &Path,
    project: &crate::Project,
    entries: Vec<Replacement>,
    expected: &std::collections::HashMap<PathBuf, Option<Vec<u8>>>,
    migration_ownership: Option<&super::ScriptMigrationOwnership>,
) -> io::Result<()> {
    let source = entries
        .iter()
        .find(|entry| entry.expected.is_some() && entry.desired.is_none())
        .ok_or_else(|| io::Error::other("Script migration has no retired source"))?
        .path
        .clone();
    let destination = entries
        .iter()
        .find(|entry| {
            entry.expected.is_none()
                && entry.desired.is_some()
                && entry.path.extension() == source.extension()
        })
        .ok_or_else(|| io::Error::other("Script migration has no new source"))?
        .path
        .clone();
    let root_expected = expected
        .get(&project.file_location)
        .and_then(Option::as_ref)
        .ok_or_else(|| conflict(&project.file_location))?;
    let mut project_guards: Vec<_> = expected
        .iter()
        .filter(|(path, _)| {
            (crate::Project::is_project_file(path)
                || migration_ownership
                    .is_some_and(|ownership| ownership.declarations.contains_key(*path)))
                && !entries.iter().any(|entry| entry.path == **path)
        })
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();
    project_guards.sort_by(|(a, _), (b, _)| a.cmp(b));
    let has_packages = directory.ancestors().skip(1).any(|ancestor| {
        vfs.metadata(ancestor.join("wally.toml"))
            .with_not_found()
            .is_ok_and(|value| value.is_some())
    });
    let journal = ReplacementJournal {
        version: JOURNAL_VERSION,
        kind: "replacement".into(),
        entries,
        ownership: has_packages
            .then(|| ReplacementOwnership::capture(vfs, directory))
            .transpose()?,
        script: Some(ScriptReplacementEvidence {
            project: project.file_location.clone(),
            expected: VfsSnapshot::file(root_expected.clone()),
            source,
            destination,
            project_guards,
            migration_ownership: migration_ownership.cloned(),
        }),
    };
    execute_replacement_journal(vfs, directory, journal, expected)
}

fn execute_replacement_journal(
    vfs: &Vfs,
    directory: &Path,
    journal: ReplacementJournal,
    expected: &std::collections::HashMap<PathBuf, Option<Vec<u8>>>,
) -> io::Result<()> {
    validate_replacements(&vfs.lock(), directory, &journal)?;
    if let Some(ownership) = &journal.ownership {
        for entry in &journal.entries {
            if let Some(reason) = ownership.policy.blocked(vfs, &entry.path, true) {
                return Err(recovery_refused(directory, &reason));
            }
        }
    }
    let mut fs = vfs.lock();
    // Source, metadata, competing initializers, and import inputs share the same
    // byte guard as scoped writes, before publishing any destructive operation.
    for (path, contents) in expected {
        if fs
            .read(path)
            .with_not_found()?
            .as_ref()
            .map(|bytes| bytes.as_slice())
            != contents.as_deref()
        {
            return Err(conflict(path));
        }
    }
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
                } else if let Some(script) = journal
                    .script
                    .as_ref()
                    .filter(|script| script.destination == entry.path)
                {
                    fs.copy_permissions(&script.source, &output)?;
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
    validate_replacements(fs, directory, journal)?;
    let committed = marker(fs, directory, "commit", COMMIT)?;
    let rolled_back = marker(fs, directory, "rollback", ROLLBACK)?;
    if committed && rolled_back {
        return Err(io::Error::other(
            "Contradictory replacement journal markers",
        ));
    }
    // Only an unfinished journal rolls project files back. Finished journals touch
    // nothing outside their directory, and later project edits must not block them.
    if !committed && !rolled_back {
        validate_replacement_ownership(fs, directory, journal)?;
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
    validate_path_layout(fs, paths)?;
    for path in paths {
        let parent = path.parent().unwrap();
        // Recovery must revalidate ancestors too. A previously canonical path can
        // point outside the project after an editor replaces a parent with a link.
        require_canonical_parent(fs, path)?;
        if check_parents
            && (!same_canonical_spelling(parent, &fs.normalize(parent)?)
                || !fs.metadata(parent)?.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Relocation parent must be a canonical existing directory",
            ));
        }
    }
    Ok(())
}

// Read-only guards share path isolation rules, but not write-parent requirements.
fn validate_path_layout(fs: &VfsLock<'_>, paths: &[&Path]) -> io::Result<()> {
    let mut sorted = Vec::with_capacity(paths.len());
    // Defer the first malformed-path error until the preceding valid paths have
    // been checked for overlap, preserving the original input-order error priority.
    let validation = (|| {
        for path in paths {
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
            if path.parent().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Cannot relocate filesystem roots",
                ));
            }
            sorted.push(*path);
        }
        Ok(())
    })();
    // Path ordering, like starts_with, compares components, not raw OS strings.
    // A prefix precedes its descendants, which form a contiguous sorted interval.
    // Thus any overlapping pair implies an adjacent overlapping pair (including
    // duplicates). Keep both argument orders and the native prefix/separator rules;
    // canonical namespace equivalence must not broaden lexical isolation here.
    sorted.sort_unstable_by(|left, right| {
        #[cfg(test)]
        tests::LAYOUT_COMPARISONS.with(|count| count.set(count.get() + 1));
        left.cmp(right)
    });
    for pair in sorted.windows(2) {
        #[cfg(test)]
        tests::LAYOUT_COMPARISONS.with(|count| count.set(count.get() + 1));
        if pair[0].starts_with(pair[1]) || pair[1].starts_with(pair[0]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Relocation paths overlap",
            ));
        }
    }
    validation
}

#[cfg(test)]
mod tests {
    use super::*;
    use memofs::{InMemoryFs, StdBackend, VfsBackend};

    thread_local! {
        pub(super) static LAYOUT_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(super) static DECLARATION_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    // Keep the original quadratic predicate independent of the production algorithm.
    fn project_guard_layout_oracle(fs: &VfsLock<'_>, paths: &[&Path]) -> bool {
        for (index, path) in paths.iter().enumerate() {
            if !fs.is_absolute_path(path)
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
                || path.parent().is_none()
            {
                return false;
            }
            for other in &paths[..index] {
                if path.starts_with(other) || other.starts_with(path) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn project_guard_layout_matches_exhaustive_ordered_oracle() {
        let candidates: Vec<_> = [
            "", ".", "/", "relative", "/a", "/a/b", "/a-b", "/a/bc", "/a/./b", "/a/../b", "/a//b/",
            "/A", "/a\\b",
        ]
        .iter()
        .map(Path::new)
        .collect();
        let memory = Vfs::new(InMemoryFs::new());
        let native = Vfs::new(StdBackend::new().unwrap());
        for vfs in [&memory, &native] {
            let fs = vfs.lock();
            for length in 0..=4 {
                for mut value in 0..candidates.len().pow(length) {
                    let mut paths = Vec::new();
                    for _ in 0..length {
                        paths.push(candidates[value % candidates.len()]);
                        value /= candidates.len();
                    }
                    assert_eq!(
                        validate_path_layout(&fs, &paths).is_ok(),
                        project_guard_layout_oracle(&fs, &paths),
                        "Ordered input {paths:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn project_guard_layout_matches_namespace_and_component_oracle() {
        let candidates: Vec<_> = [
            "/project/a",
            "/project/a/child",
            "/project/a-else",
            "/project/ab",
            "/project/a/",
            "/project//a",
            "/project/A",
            "/project/é",
            "/project/é/child",
            r"C:\project\a",
            r"C:/project/a",
            r"C:\project\a\child",
            r"C:\project\ab",
            r"C:project\a",
            r"\project\a",
            r"c:\project\a",
            r"D:\project\a",
            r"\\?\C:\project\a",
            r"\\?\C:\project\a\child",
            r"\\?\C:\project\a/child",
            r"\\server\share\a",
            r"//server/share/a",
            r"\\server\share\a\child",
            r"\\?\UNC\server\share\a",
            r"\\?\UNC\server\share\a\child",
            r"\\.\device\a",
            r"\\.\device\a\child",
            r"\\?\volume\a",
        ]
        .iter()
        .map(Path::new)
        .collect();
        let memory = Vfs::new(InMemoryFs::new());
        let native = Vfs::new(StdBackend::new().unwrap());
        for vfs in [&memory, &native] {
            let fs = vfs.lock();
            for a in &candidates {
                for b in &candidates {
                    for c in &candidates {
                        let paths = [*a, *b, *c];
                        assert_eq!(
                            validate_path_layout(&fs, &paths).is_ok(),
                            project_guard_layout_oracle(&fs, &paths),
                            "Namespace input {paths:?}"
                        );
                    }
                }
            }
        }
    }

    fn project_guard_fixture(
        real: bool,
        count: usize,
    ) -> (Vfs, tempfile::TempDir, PathBuf, ReplacementJournal) {
        let (fs, temp, root) = fixture(real);
        let directory = root.join("cache/transaction");
        fs.create_dir_all(directory.parent().unwrap()).unwrap();
        let mut ownership = super::super::ScriptMigrationOwnership::default();
        let mut guards = Vec::new();
        for index in 0..count {
            let path = root.join(format!("guard-{index:05}.project.json"));
            let bytes = (index % 2 == 0).then(|| format!("declaration {index}").into_bytes());
            if let Some(bytes) = &bytes {
                fs.write(&path, bytes).unwrap();
            }
            ownership.declarations.insert(path.clone(), bytes.clone());
            guards.push((path, bytes));
        }
        let journal = ReplacementJournal {
            version: JOURNAL_VERSION,
            kind: "replacement".into(),
            entries: (0..3)
                .map(|index| Replacement {
                    path: root.join(format!("write-{index}.luau")),
                    expected: None,
                    desired: Some(VfsSnapshot::file("output")),
                })
                .collect(),
            ownership: None,
            script: Some(ScriptReplacementEvidence {
                project: guards[0].0.clone(),
                expected: VfsSnapshot::file(guards[0].1.clone().unwrap()),
                source: root.join("write-0.luau"),
                destination: root.join("write-1.luau"),
                project_guards: guards,
                migration_ownership: Some(ownership),
            }),
        };
        (fs, temp, directory, journal)
    }

    #[test]
    fn project_guard_layout_comparison_budget() {
        let memory = Vfs::new(InMemoryFs::new());
        for count in [256, 1024] {
            let owned: Vec<_> = (0..count)
                .rev()
                .map(|index| PathBuf::from(format!("/guard-{index:05}.project.json")))
                .collect();
            let paths: Vec<_> = owned.iter().map(PathBuf::as_path).collect();
            LAYOUT_COMPARISONS.with(|value| value.set(0));
            validate_path_layout(&memory.lock(), &paths).unwrap();
            let comparisons = LAYOUT_COMPARISONS.with(std::cell::Cell::get);
            assert!(
                comparisons <= count * 32,
                "{count} paths used {comparisons} comparisons"
            );
        }
    }

    #[test]
    fn project_guard_declaration_probe_budget() {
        let count = 1024;
        let (fs, _temp, directory, journal) = project_guard_fixture(false, count);
        DECLARATION_PROBES.with(|value| value.set(0));
        validate_script_project_guards(
            &mut fs.lock(),
            &directory,
            &journal,
            journal.script.as_ref().unwrap(),
        )
        .unwrap();
        let probes = DECLARATION_PROBES.with(std::cell::Cell::get);
        assert!(
            probes <= count * 2,
            "{count} declarations used {probes} probes"
        );
    }

    fn project_guard_copy_script(journal: &ReplacementJournal) -> ScriptReplacementEvidence {
        serde_json::from_value(serde_json::to_value(journal.script.as_ref().unwrap()).unwrap())
            .unwrap()
    }

    #[test]
    fn project_guard_declaration_exact_bytes_and_absence() {
        for real in [false, true] {
            let (fs, _temp, directory, journal) = project_guard_fixture(real, 2);
            for replacement in [
                None,
                Some(Vec::new()),
                Some(b"declaration 0\0".to_vec()),
                Some(b"declaration 0".to_vec()),
            ] {
                for index in 0..2 {
                    let mut script = project_guard_copy_script(&journal);
                    script.project_guards[index].1 = replacement.clone();
                    let expected =
                        replacement == journal.script.as_ref().unwrap().project_guards[index].1;
                    assert_eq!(
                        validate_script_project_guards(
                            &mut fs.lock(),
                            &directory,
                            &journal,
                            &script
                        )
                        .is_ok(),
                        expected
                    );
                }
            }
            let mut script = project_guard_copy_script(&journal);
            script.project_guards.pop();
            assert!(
                validate_script_project_guards(&mut fs.lock(), &directory, &journal, &script)
                    .is_err()
            );
        }
    }

    #[test]
    fn project_guard_duplicate_write_and_journal_overlap() {
        for real in [false, true] {
            let (fs, _temp, directory, journal) = project_guard_fixture(real, 2);
            for path in [
                journal.script.as_ref().unwrap().project_guards[1].0.clone(),
                journal.entries[0].path.clone(),
                journal.entries[0].path.join("nested.project.json"),
                directory.clone(),
                directory.join("guard.project.json"),
                directory.parent().unwrap().to_path_buf(),
            ] {
                let mut script = project_guard_copy_script(&journal);
                script.project_guards.push((path, None));
                let error =
                    validate_script_project_guards(&mut fs.lock(), &directory, &journal, &script)
                        .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                assert_eq!(error.to_string(), "Relocation paths overlap");
            }
        }
    }

    #[derive(Clone, Copy)]
    struct OwnershipValidationShape {
        directories: usize,
        entries: usize,
        guard_parents: usize,
        present: usize,
    }

    struct OwnershipValidationFixture {
        baseline: Vfs,
        recorded: Vfs,
        handle: memofs::test_support::RecordingHandle,
        _temp: tempfile::TempDir,
        directory: PathBuf,
        journal: ReplacementJournal,
    }

    fn ownership_validation_fixture(
        real: bool,
        shape: OwnershipValidationShape,
    ) -> OwnershipValidationFixture {
        use memofs::test_support::RecordingBackend;

        assert!(shape.guard_parents > shape.directories);
        assert!(shape.present > 0);
        assert!(shape.entries >= shape.directories * 3);
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let (baseline, recorded, handle) = if real {
            let (backend, handle) = RecordingBackend::new(StdBackend::new().unwrap());
            (
                Vfs::new(StdBackend::new().unwrap()),
                Vfs::new(backend),
                handle,
            )
        } else {
            let backend = InMemoryFs::new();
            let (recorded, handle) = RecordingBackend::new(backend.clone());
            (Vfs::new(backend), Vfs::new(recorded), handle)
        };
        for fs in [&baseline, &recorded] {
            fs.set_watch_enabled(false);
        }
        baseline
            .create_dir_all(root.join(".rojo/relocations"))
            .unwrap();
        let directory = root.join(".rojo/relocations/measurement");
        let mut ownership = super::super::ScriptMigrationOwnership::default();
        let mut parents = Vec::new();
        let mut writes = Vec::new();
        // Grouped generic ASCII paths keep parent inventories small. These are
        // synthetic evidence choices, not a replay of an installed project graph.
        for index in 0..shape.directories {
            let parent = root.join(format!("mapped/group-{:03}/node-{index:05}", index / 100));
            baseline.create_dir_all(&parent).unwrap();
            ownership.identity(&baseline, &parent).unwrap();
            if index == 0 {
                writes = ["Script.server.luau", "Script.luau", "Script.meta.json"]
                    .map(|name| parent.join(name))
                    .to_vec();
                baseline.write(&writes[0], "print('source')").unwrap();
                baseline.write(&writes[2], "{}").unwrap();
            }
            let entries = shape.entries / shape.directories
                + usize::from(index < shape.entries % shape.directories);
            let existing = if index == 0 { 2 } else { 0 };
            for slot in existing..entries {
                baseline
                    .write(parent.join(format!("data-{slot}.luau")), "return nil")
                    .unwrap();
            }
            let mut names = Vec::new();
            for entry in baseline.read_dir(&parent).unwrap() {
                let path = entry.unwrap().path().to_owned();
                names.push(PathBuf::from(path.file_name().unwrap()));
                ownership.files.insert(path.clone());
                ownership.identity(&baseline, &path).unwrap();
            }
            names.sort();
            ownership.directories.insert(parent.clone(), names);
            parents.push(parent);
        }
        // Two default-declaration absence probes in each captured inventory; one
        // optional mapping probe in each remaining parent. All parents exist here.
        for index in shape.directories..shape.guard_parents {
            let parent = if index == shape.directories {
                root.clone()
            } else {
                root.join(format!("declarations/ref-{index:05}"))
            };
            baseline.create_dir_all(&parent).unwrap();
            parents.push(parent);
        }
        for (index, parent) in parents.iter().enumerate() {
            let probes: &[&str] = if index < shape.directories {
                &["default.project.json", "default.project.jsonc"]
            } else {
                &["optional.project.json"]
            };
            for probe in probes {
                let path = parent.join(probe);
                assert!(ownership.capture(&baseline, &path).unwrap().is_none());
                if index >= shape.directories {
                    ownership.absent.insert(path);
                }
            }
        }
        let project = root.join("main-000.project.json");
        for index in 0..shape.present {
            let parent =
                &parents[shape.directories + index % (shape.guard_parents - shape.directories)];
            let path = parent.join(format!("main-{index:03}.project.json"));
            baseline
                .write(
                    &path,
                    br#"{"name":"Measurement","tree":{"$className":"Folder"}}"#,
                )
                .unwrap();
            ownership.capture(&baseline, &path).unwrap().unwrap();
            ownership.files.insert(path);
        }
        for path in &writes {
            ownership.identity(&baseline, path).unwrap();
        }
        ownership.write_authority = Some(super::super::ScriptMigrationWriteAuthority {
            source: writes[0].clone(),
            destination: writes[1].clone(),
            paths: writes.iter().cloned().collect(),
        });
        let mut guards: Vec<_> = ownership
            .declarations
            .iter()
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect();
        guards.sort_by(|a, b| a.0.cmp(&b.0));
        let journal = ReplacementJournal {
            version: JOURNAL_VERSION,
            kind: "replacement".into(),
            entries: vec![
                Replacement {
                    path: writes[0].clone(),
                    expected: Some(VfsSnapshot::file("print('source')")),
                    desired: None,
                },
                Replacement {
                    path: writes[1].clone(),
                    expected: None,
                    desired: Some(VfsSnapshot::file("print('destination')")),
                },
                Replacement {
                    path: writes[2].clone(),
                    expected: Some(VfsSnapshot::file("{}")),
                    desired: Some(VfsSnapshot::file(
                        r#"{"properties":{"RunContext":"Client"}}"#,
                    )),
                },
            ],
            ownership: None,
            script: Some(ScriptReplacementEvidence {
                expected: VfsSnapshot::file(ownership.declarations[&project].clone().unwrap()),
                project,
                source: writes[0].clone(),
                destination: writes[1].clone(),
                project_guards: guards,
                migration_ownership: Some(ownership),
            }),
        };
        // Round-trip the durable authority evidence rather than recapturing it
        // during validation.
        let journal: ReplacementJournal =
            serde_json::from_slice(&serde_json::to_vec(&journal).unwrap()).unwrap();
        let script = journal.script.as_ref().unwrap();
        let ownership = script.migration_ownership.as_ref().unwrap();
        let absent = script
            .project_guards
            .iter()
            .filter(|(_, bytes)| bytes.is_none())
            .count();
        let mut per_parent = std::collections::BTreeMap::new();
        for (path, bytes) in &script.project_guards {
            if bytes.is_none() {
                *per_parent.entry(path.parent().unwrap()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            absent,
            shape.directories * 2 + shape.guard_parents - shape.directories
        );
        assert_eq!(script.project_guards.len(), absent + shape.present);
        assert_eq!(per_parent.len(), shape.guard_parents);
        assert_eq!(per_parent.values().copied().max(), Some(2));
        assert_eq!(ownership.directories.len(), shape.directories);
        assert_eq!(
            ownership.directories.values().map(Vec::len).sum::<usize>(),
            shape.entries
        );
        assert_eq!(ownership.write_authority.as_ref().unwrap().paths.len(), 3);
        assert!(handle.snapshot().is_empty());
        OwnershipValidationFixture {
            baseline,
            recorded,
            handle,
            _temp: temp,
            directory,
            journal,
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct OwnershipValidationSummary {
        calls: u64,
        unique_paths: usize,
        largest_repeat: u64,
    }

    fn ownership_validation_summary(
        counts: &memofs::test_support::BackendCounts,
    ) -> std::collections::BTreeMap<&'static str, OwnershipValidationSummary> {
        let mut summary = std::collections::BTreeMap::new();
        for operation in counts
            .keys()
            .map(|call| call.operation)
            .collect::<std::collections::BTreeSet<_>>()
        {
            let mut calls = 0;
            let mut paths = std::collections::BTreeSet::new();
            let mut largest_repeat = 0;
            for (call, count) in counts
                .iter()
                .filter(|(call, _)| call.operation == operation)
            {
                calls += count;
                paths.extend(call.paths.iter().cloned());
                largest_repeat = largest_repeat.max(*count);
            }
            summary.insert(
                operation,
                OwnershipValidationSummary {
                    calls,
                    unique_paths: paths.len(),
                    largest_repeat,
                },
            );
        }
        summary
    }

    fn ownership_validation_validate(
        fixture: &OwnershipValidationFixture,
        fs: &Vfs,
        full: bool,
    ) -> io::Result<()> {
        if full {
            validate_replacement_ownership(&mut fs.lock(), &fixture.directory, &fixture.journal)
        } else {
            let writes: Vec<_> = fixture
                .journal
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect();
            fixture
                .journal
                .script
                .as_ref()
                .unwrap()
                .migration_ownership
                .as_ref()
                .unwrap()
                .validate(&mut fs.lock(), &writes)
        }
    }

    fn ownership_validation_counts(
        real: bool,
        shape: OwnershipValidationShape,
    ) -> Vec<std::collections::BTreeMap<&'static str, OwnershipValidationSummary>> {
        let mut fixture = ownership_validation_fixture(real, shape);
        // Legacy journals lack separately frozen lexical write-parent evidence.
        // Keep the exhaustive fallback as a recording-equivalence control.
        let parent = fixture.journal.entries[0].path.parent().unwrap().to_owned();
        fixture
            .journal
            .script
            .as_mut()
            .unwrap()
            .migration_ownership
            .as_mut()
            .unwrap()
            .paths
            .remove(&parent);
        let mut summaries = Vec::new();
        for (full, segment) in [
            (false, "migration_ownership"),
            (true, "full_replacement_ownership"),
        ] {
            let baseline = ownership_validation_validate(&fixture, &fixture.baseline, full);
            fixture.handle.start();
            let recorded = ownership_validation_validate(&fixture, &fixture.recorded, full);
            let counts = fixture.handle.stop();
            assert!(
                baseline.is_ok(),
                "Baseline must accept {segment} {baseline:?}"
            );
            assert!(
                recorded.is_ok(),
                "Recorder must accept {segment} {recorded:?}"
            );
            assert!(!counts.is_empty());
            assert!(counts.keys().all(|call| matches!(
                call.operation,
                "canonicalize" | "is_absolute_path" | "metadata" | "read" | "read_dir"
            )));
            summaries.push(ownership_validation_summary(&counts));
        }
        summaries
    }

    #[test]
    fn distributed_ownership_backend_counts_match_native_and_memory() {
        let shape = OwnershipValidationShape {
            directories: 4,
            entries: 13,
            guard_parents: 6,
            present: 3,
        };
        let memory = ownership_validation_counts(false, shape);
        let native = ownership_validation_counts(true, shape);
        assert_eq!(memory, native);
        for summary in &memory {
            for operation in ["canonicalize", "read_dir", "metadata", "read"] {
                assert!(summary[operation].calls > 0);
                assert!(summary[operation].unique_paths > 0);
                assert!(summary[operation].largest_repeat > 0);
            }
        }
        // Removing one frozen parent removes exactly its one identity probe, not
        // any of the exhaustive directory-by-write observations measured here.
        assert_eq!(memory[0]["canonicalize"].calls + 1, 58);
        assert_eq!(memory[1]["canonicalize"].calls + 1, 88);
        assert_eq!(memory[0]["canonicalize"].largest_repeat + 1, 17);
        assert_eq!(memory[1]["canonicalize"].largest_repeat + 1, 22);
        assert_eq!(memory[0]["metadata"].calls, 16);
        assert_eq!(memory[0]["read"].calls, 13);
        assert_eq!(memory[1]["read"].calls, 26);
        assert_eq!(memory[0]["read_dir"].calls, 15);
        assert_eq!(memory[1]["read_dir"].calls, 25);
        assert_eq!(memory[0]["read_dir"].unique_paths, 6);
        assert_eq!(memory[0]["read_dir"].largest_repeat, 4);
    }

    fn selective_write_parents_count(
        counts: &memofs::test_support::BackendCounts,
        operation: &str,
        path: Option<&Path>,
    ) -> u64 {
        counts
            .iter()
            .filter(|(call, _)| {
                call.operation == operation
                    && path.is_none_or(|path| call.paths == [path.as_os_str()])
            })
            .map(|(_, count)| count)
            .sum()
    }

    #[test]
    fn selective_write_parents_distributed_operation_budget_and_incomplete_control() {
        for real in [false, true] {
            for directories in [64, 4] {
                let mut fixture = ownership_validation_fixture(
                    real,
                    OwnershipValidationShape {
                        directories,
                        entries: directories * 3 + 1,
                        guard_parents: directories + 2,
                        present: 3,
                    },
                );
                let parent = fixture.journal.entries[0].path.parent().unwrap().to_owned();
                for full in [false, true] {
                    fixture.handle.start();
                    ownership_validation_validate(&fixture, &fixture.recorded, full).unwrap();
                    let optimized = fixture.handle.stop();
                    let count =
                        selective_write_parents_count(&optimized, "canonicalize", Some(&parent));
                    let budget = if full { 19 } else { 14 };
                    assert!(count <= budget, "Write-parent normalization exceeded its budget with real={real}, directories={directories}, full={full}, calls={count}, budget={budget}.");
                    let ownership = fixture
                        .journal
                        .script
                        .as_mut()
                        .unwrap()
                        .migration_ownership
                        .as_mut()
                        .unwrap();
                    let frozen = ownership.paths.remove(&parent).unwrap();
                    fixture.handle.start();
                    ownership_validation_validate(&fixture, &fixture.recorded, full).unwrap();
                    let fallback = fixture.handle.stop();
                    fixture
                        .journal
                        .script
                        .as_mut()
                        .unwrap()
                        .migration_ownership
                        .as_mut()
                        .unwrap()
                        .paths
                        .insert(parent.clone(), frozen);
                    // The control loses one identity observation but still probes
                    // every actual write parent at every captured directory.
                    assert_eq!(
                        selective_write_parents_count(&fallback, "canonicalize", Some(&parent)),
                        (directories * 3 + if full { 9 } else { 4 }) as u64
                    );
                    for operation in ["read", "read_dir", "metadata"] {
                        assert_eq!(
                            selective_write_parents_count(&optimized, operation, None),
                            selective_write_parents_count(&fallback, operation, None)
                        );
                    }
                    assert_eq!(
                        selective_write_parents_count(&fallback, "canonicalize", None),
                        selective_write_parents_count(&optimized, "canonicalize", None)
                            + (3 * (directories - 1)) as u64
                            - 7
                    );
                }
            }
        }
    }

    #[test]
    fn selective_write_parents_empty_writes_keep_traversal_without_exemptions() {
        let fixture = ownership_validation_fixture(
            false,
            OwnershipValidationShape {
                directories: 4,
                entries: 13,
                guard_parents: 6,
                present: 3,
            },
        );
        let ownership = fixture
            .journal
            .script
            .as_ref()
            .unwrap()
            .migration_ownership
            .as_ref()
            .unwrap();
        fixture.handle.start();
        ownership
            .validate(&mut fixture.recorded.lock(), &[])
            .unwrap();
        let counts = fixture.handle.stop();
        assert_eq!(
            selective_write_parents_count(
                &counts,
                "canonicalize",
                Some(fixture.journal.entries[0].path.parent().unwrap())
            ),
            5
        );
        fixture
            .baseline
            .remove_file(&fixture.journal.entries[0].path)
            .unwrap();
        assert!(ownership
            .validate(&mut fixture.recorded.lock(), &[])
            .is_err());
    }

    #[test]
    fn distributed_ownership_recording_preserves_rejections_and_frozen_authority() {
        for real in [false, true] {
            let mut fixture = ownership_validation_fixture(
                real,
                OwnershipValidationShape {
                    directories: 2,
                    entries: 6,
                    guard_parents: 3,
                    present: 1,
                },
            );
            for full in [false, true] {
                let path = &fixture.journal.script.as_ref().unwrap().project;
                let bytes = fixture.baseline.read(path).unwrap();
                fixture.baseline.write(path, b"changed").unwrap();
                let baseline =
                    ownership_validation_validate(&fixture, &fixture.baseline, full).unwrap_err();
                fixture.handle.start();
                let recorded =
                    ownership_validation_validate(&fixture, &fixture.recorded, full).unwrap_err();
                assert!(!fixture.handle.stop().is_empty());
                assert_eq!(baseline.kind(), recorded.kind());
                assert_eq!(baseline.to_string(), recorded.to_string());
                fixture.baseline.write(path, bytes.as_slice()).unwrap();
            }
            fixture
                .journal
                .script
                .as_mut()
                .unwrap()
                .migration_ownership
                .as_mut()
                .unwrap()
                .write_authority
                .as_mut()
                .unwrap()
                .paths
                .remove(&fixture.journal.entries[2].path);
            let baseline =
                ownership_validation_validate(&fixture, &fixture.baseline, true).unwrap_err();
            fixture.handle.start();
            let recorded =
                ownership_validation_validate(&fixture, &fixture.recorded, true).unwrap_err();
            assert!(!fixture.handle.stop().is_empty());
            assert_eq!(baseline.kind(), recorded.kind());
            assert_eq!(baseline.to_string(), recorded.to_string());
        }
    }

    #[test]
    fn replacement_validation_uses_the_backend_path_namespace() {
        let directory = Path::new("/project/cache/transaction");
        let mut journal = ReplacementJournal {
            version: JOURNAL_VERSION,
            kind: "replacement".into(),
            entries: vec![Replacement {
                path: PathBuf::from("/project/script.server.luau"),
                expected: Some(VfsSnapshot::file("source")),
                desired: None,
            }],
            ownership: None,
            script: None,
        };
        let memory = Vfs::new(InMemoryFs::new());
        assert!(validate_replacements(&memory.lock(), directory, &journal).is_ok());
        let native = Vfs::new(StdBackend::new().unwrap());
        assert_eq!(
            validate_replacements(&native.lock(), directory, &journal).is_ok(),
            journal.entries[0].path.is_absolute()
        );
        for invalid in [
            "project/script.server.luau",
            "/project/../script.server.luau",
            "/project/cache/transaction/output",
            "/project/cache",
        ] {
            journal.entries[0].path = invalid.into();
            assert!(validate_replacements(&memory.lock(), directory, &journal).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn canonical_spelling_allows_only_equivalent_windows_namespace_prefixes() {
        for (path, canonical, expected) in [
            (r"C:\project\deep", r"\\?\C:\project\deep", true),
            (r"\\?\C:\project\deep", r"C:\project\deep", true),
            (r"\\server\share\deep", r"\\?\UNC\server\share\deep", true),
            (r"C:\project\deep", r"\\?\D:\project\deep", false),
            (r"C:\project\alias", r"\\?\C:\project\target", false),
            (r"C:\project\name.", r"\\?\C:\project\name.", false),
            (r"C:\project\name ", r"\\?\C:\project\name ", false),
            (r"C:\project\CON", r"\\?\C:\project\CON", false),
            (r"C:\project\a\..\deep", r"\\?\C:\project\deep", false),
            (r"C:project\deep", r"\\?\C:\project\deep", false),
            (r"\\server\share\deep", r"\\?\UNC\other\share\deep", false),
        ] {
            assert_eq!(
                same_canonical_spelling(Path::new(path), Path::new(canonical)),
                expected
            );
        }
    }

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
    fn script_replacement_requires_unchanged_project_and_package_evidence() {
        for packages in [false, true] {
            let (fs, _temp, root) = fixture(true);
            if packages {
                package_fixture(&fs, &root);
            }
            let project_path = root.join("default.project.json");
            fs.write(
                &project_path,
                r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#,
            )
            .unwrap();
            let project = crate::Project::load_exact(&fs, &project_path, None).unwrap();
            let source = root.join("src/Code.legacy.lua");
            let destination = root.join("src/Code.client.lua");
            fs.write(&source, "before").unwrap();
            let directory = root.join(".rojo/relocations/script");
            fs.create_dir_all(directory.parent().unwrap()).unwrap();
            execute_script_replacements(
                &fs,
                &directory,
                &project,
                vec![
                    Replacement {
                        path: source.clone(),
                        expected: Some(VfsSnapshot::file("before")),
                        desired: None,
                    },
                    Replacement {
                        path: destination.clone(),
                        expected: None,
                        desired: Some(VfsSnapshot::file("after")),
                    },
                ],
                &std::collections::HashMap::from([(
                    project_path.clone(),
                    Some(fs.read(&project_path).unwrap().as_ref().clone()),
                )]),
            )
            .unwrap();
            let saved: ReplacementJournal =
                serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap()).unwrap();
            assert_eq!(saved.ownership.is_some(), packages);
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
            fs.remove_file(directory.join("commit")).unwrap();
            if packages {
                fs.write(root.join("wally.toml"), "[dependencies]\n")
                    .unwrap();
            } else {
                fs.write(&project_path, "{}").unwrap();
            }
            let before = fs.snapshot_tree(&root).unwrap();
            assert!(recover(&fs, &directory).is_err());
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn script_replacement_preserves_source_permissions_at_renamed_destination() {
        use std::os::unix::fs::PermissionsExt;
        let (fs, _temp, root) = fixture(true);
        let project_path = root.join("default.project.json");
        fs.write(
            &project_path,
            r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#,
        )
        .unwrap();
        let project = crate::Project::load_exact(&fs, &project_path, None).unwrap();
        let source = root.join("src/Code.legacy.lua");
        let destination = root.join("src/Code.client.lua");
        fs.write(&source, "before").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        let directory = root.join(".rojo/relocations/script");
        fs.create_dir_all(directory.parent().unwrap()).unwrap();
        execute_script_replacements(
            &fs,
            &directory,
            &project,
            vec![
                Replacement {
                    path: source,
                    expected: Some(VfsSnapshot::file("before")),
                    desired: None,
                },
                Replacement {
                    path: destination.clone(),
                    expected: None,
                    desired: Some(VfsSnapshot::file("after")),
                },
            ],
            &std::collections::HashMap::from([(
                project_path.clone(),
                Some(fs.read(&project_path).unwrap().as_ref().clone()),
            )]),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn script_replacement_restart_recovers_source_and_sidecar_at_every_boundary() {
        for real in [false, true] {
            for phase in 0..8 {
                let (fs, _temp, root) = fixture(real);
                let project = root.join("default.project.json");
                fs.write(
                    &project,
                    r#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#,
                )
                .unwrap();
                let directory = root.join(".rojo/relocations/script");
                fs.create_dir_all(directory.parent().unwrap()).unwrap();
                let journal = ReplacementJournal {
                    version: JOURNAL_VERSION,
                    kind: "replacement".into(),
                    ownership: None,
                    script: Some(ScriptReplacementEvidence {
                        project: project.clone(),
                        expected: fs.snapshot_tree(&project).unwrap(),
                        source: root.join("src/Code.legacy.lua"),
                        destination: root.join("src/Code.client.lua"),
                        project_guards: vec![(
                            project.clone(),
                            Some(fs.read(&project).unwrap().as_ref().clone()),
                        )],
                        migration_ownership: None,
                    }),
                    entries: vec![
                        Replacement {
                            path: root.join("src/Code.legacy.lua"),
                            expected: Some(VfsSnapshot::file("old source")),
                            desired: None,
                        },
                        Replacement {
                            path: root.join("src/Code.client.lua"),
                            expected: None,
                            desired: Some(VfsSnapshot::file("new source")),
                        },
                        Replacement {
                            path: root.join("src/Code.meta.jsonc"),
                            expected: Some(VfsSnapshot::file("old meta")),
                            desired: Some(VfsSnapshot::file("new meta")),
                        },
                    ],
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
                    for (threshold, index) in [(1, 1), (2, 2)] {
                        if phase >= threshold {
                            materialize(
                                &mut lock,
                                &directory.join(format!("output-{index}")),
                                journal.entries[index].desired.as_ref().unwrap(),
                            )
                            .unwrap();
                        }
                    }
                    for (threshold, index) in [(3, 0), (4, 2)] {
                        if phase >= threshold {
                            transfer(
                                &mut lock,
                                &journal.entries[index].path,
                                &stage(&directory, index),
                                journal.entries[index].expected.as_ref().unwrap(),
                            )
                            .unwrap();
                        }
                    }
                    for (threshold, index) in [(5, 1), (6, 2)] {
                        if phase >= threshold {
                            transfer(
                                &mut lock,
                                &directory.join(format!("output-{index}")),
                                &journal.entries[index].path,
                                journal.entries[index].desired.as_ref().unwrap(),
                            )
                            .unwrap();
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
                let expected = if phase == 7 {
                    Recovery::Committed
                } else {
                    Recovery::RolledBack
                };
                assert_eq!(
                    recover(&restarted, &directory).unwrap(),
                    expected,
                    "phase {phase}, real {real}"
                );
                assert_eq!(recover(&restarted, &directory).unwrap(), expected);
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

    fn prepared_script_migration_fixture(
        real: bool,
        reverse: bool,
        nested: bool,
    ) -> (Vfs, tempfile::TempDir, PathBuf, ReplacementJournal) {
        use rbx_dom_weak::ustr;
        use rbx_dom_weak::{
            types::{Enum, Variant},
            InstanceBuilder, WeakDom,
        };

        let (fs, temp, root) = fixture(real);
        let config = if nested {
            root.join(".local/config")
        } else {
            root.clone()
        };
        let sources = root.join("Source/DataModel/ReplicatedFirst");
        fs.create_dir_all(&config).unwrap();
        fs.create_dir_all(&sources).unwrap();
        let source = sources.join("LooseLuau.server.luau");
        let destination = sources.join("LooseLuau.client.luau");
        let sidecar = sources.join("LooseLuau.meta.jsonc");
        let physical = b"return \"physical LooseLuau\"\r\n";
        let effective = b"return \"effective LooseLuau\"\n";
        fs.write(&source, physical).unwrap();
        fs.write(&sidecar, concat!("// Fixture metadata.\n", "{\r\n  \"properties\": {\r\n    \"Disabled\": true,\r\n    \"Source\": \"return \\\"effective LooseLuau\\\"\\n\"\r\n  },\r\n  \"attributes\": {\"Retained\": \"LooseLuau\"},\r\n  \"name\": \"VisibleLooseLuau\"\r\n}")).unwrap();
        let project_path = config.join("place.project.json");
        let project_bytes = serde_json::to_vec(&serde_json::json!({
            "name":"Test", "tree":{"$className":"DataModel",
                "ReplicatedFirst":{"$path":sources},
                "Optional":{"$path":{"optional":root.join("missing/optional.project.json")}}}
        }))
        .unwrap();
        fs.write(&project_path, &project_bytes).unwrap();
        let project = crate::Project::load_exact(&fs, &project_path, None).unwrap();
        let mut context = crate::snapshot::InstanceContext::default();
        context.set_reserved_cache_path(config.join(".rojo"));
        let mut snapshot =
            crate::snapshot_middleware::snapshot_from_vfs(&context, &fs, &project_path)
                .unwrap()
                .unwrap();
        let require_context =
            crate::serve_session::build_require_context(&snapshot, &config).unwrap();
        crate::snapshot_middleware::prepare_requires(&mut snapshot, &fs, &require_context).unwrap();
        let tree = crate::RojoTree::new(snapshot);
        let owner = tree
            .descendants(tree.get_root_id())
            .find(|instance| instance.name() == "VisibleLooseLuau")
            .unwrap();
        assert_eq!(
            owner.properties()[&ustr("Source")],
            Variant::String(String::from_utf8(effective.to_vec()).unwrap())
        );
        assert_eq!(
            owner.properties()[&ustr("RunContext")],
            Variant::Enum(Enum::from_u32(1))
        );
        assert_eq!(owner.properties()[&ustr("Disabled")], Variant::Bool(true));
        let mut properties = owner.properties().clone();
        properties.insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(2)));
        let output = WeakDom::new(
            InstanceBuilder::new("Script")
                .with_name(owner.name())
                .with_properties(properties),
        );
        let (staged, migration) =
            crate::syncback::syncback_script_migration(&fs, &tree, &project, owner.id(), output)
                .unwrap();
        let migration = migration.unwrap();
        let mut expected = crate::syncback::FsSnapshot::capture_file_contents(
            &owner.metadata().relevant_paths,
            &fs,
        )
        .unwrap();
        expected.extend(migration.ownership.declarations.clone());
        expected.insert(destination.clone(), None);
        // Reproduce the ordered entry builder used by write_script_migration_to_vfs,
        // rather than assuming source, destination, sidecar order as older tests do.
        let mut changes = std::collections::BTreeMap::new();
        for (path, desired) in staged.file_changes() {
            let path = project.folder_location().join(path);
            let before = expected
                .get(&path)
                .unwrap_or_else(|| panic!("Missing prepared baseline at {path:?}"));
            if before.as_deref() != desired {
                changes.insert(path, (before, desired));
            }
        }
        let mut entries: Vec<_> = changes
            .into_iter()
            .map(|(path, (before, after))| Replacement {
                path,
                expected: before
                    .as_ref()
                    .map(|bytes| VfsSnapshot::file(bytes.clone())),
                desired: after.map(|bytes| VfsSnapshot::file(bytes.to_vec())),
            })
            .collect();
        assert_eq!(entries.len(), 3, "{entries:#?}");
        assert_eq!(entries[0].path, destination, "{entries:#?}");
        assert_eq!(
            entries[0].desired,
            Some(VfsSnapshot::file(effective.as_slice())),
            "{entries:#?}"
        );
        assert_eq!(entries[2].path, source, "{entries:#?}");
        assert_eq!(
            entries[2].expected,
            Some(VfsSnapshot::file(physical.as_slice())),
            "{entries:#?}"
        );
        if reverse {
            entries.reverse();
        }
        let directory = config.join(".rojo/relocations/script-migration-fixture");
        fs.create_dir_all(directory.parent().unwrap()).unwrap();
        let mut guards: Vec<_> = expected
            .iter()
            .filter(|(path, _)| {
                (crate::Project::is_project_file(path)
                    || migration.ownership.declarations.contains_key(*path))
                    && !entries.iter().any(|entry| entry.path == **path)
            })
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect();
        guards.sort_by(|a, b| a.0.cmp(&b.0));
        let preflight = ReplacementJournal {
            version: JOURNAL_VERSION,
            kind: "replacement".into(),
            entries: entries.clone(),
            ownership: None,
            script: Some(ScriptReplacementEvidence {
                project: project_path.clone(),
                expected: VfsSnapshot::file(project_bytes),
                source,
                destination,
                project_guards: guards,
                migration_ownership: Some(migration.ownership.clone()),
            }),
        };
        validate_replacements(&fs.lock(), &directory, &preflight).unwrap();
        for (path, bytes) in &expected {
            assert_eq!(
                fs.read(path)
                    .with_not_found()
                    .unwrap()
                    .as_deref()
                    .map(Vec::as_slice),
                bytes.as_deref(),
                "Before intent byte guard {path:?}"
            );
        }
        validate_script_project_guards(
            &mut fs.lock(),
            &directory,
            &preflight,
            preflight.script.as_ref().unwrap(),
        )
        .unwrap();
        let ownership_validation =
            validate_replacement_ownership(&mut fs.lock(), &directory, &preflight);
        let result = execute_script_replacements_guarded(
            &fs,
            &directory,
            &project,
            entries.clone(),
            &expected,
            Some(&migration.ownership),
        );
        let text = |snapshot: &VfsSnapshot| match snapshot {
            VfsSnapshot::File { contents } => String::from_utf8(contents.clone()).unwrap(),
            _ => panic!("Expected a file in the prepared script migration"),
        };
        let described: Vec<_> = entries
            .iter()
            .map(|entry| {
                (
                    &entry.path,
                    entry.expected.as_ref().map(text),
                    entry.desired.as_ref().map(text),
                    fs.snapshot_tree(&entry.path)
                        .with_not_found()
                        .unwrap()
                        .as_ref()
                        .map(text),
                )
            })
            .collect();
        let intent = fs
            .read(directory.join("intent.json"))
            .with_not_found()
            .unwrap();
        if ownership_validation.is_err() {
            assert!(
                intent.is_none(),
                "Ownership rejection must precede intent publication"
            );
            for entry in &entries {
                assert_eq!(
                    fs.snapshot_tree(&entry.path).with_not_found().unwrap(),
                    entry.expected,
                    "Pre-intent rejection mutated {entry:?}"
                );
            }
        }
        assert!(result.is_ok(), "Script migration execution real={real} reverse={reverse} nested={nested}. Production execution {result:?}. Before-intent ownership validation {ownership_validation:?}. Intent published {}. Ordered path/expected/desired/actual contents {described:?}", intent.is_some());
        let journal: ReplacementJournal =
            serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap()).unwrap();
        assert_eq!(journal.entries.len(), entries.len());
        for (entry, saved) in entries.iter().zip(&journal.entries) {
            assert_eq!(entry.path, saved.path);
            assert_eq!(entry.expected, saved.expected);
            assert_eq!(entry.desired, saved.desired);
            assert_eq!(
                fs.snapshot_tree(&entry.path).with_not_found().unwrap(),
                entry.desired,
                "{entries:#?}"
            );
        }
        (fs, temp, directory, journal)
    }

    #[test]
    fn script_migration_recovery_nested_preintent_memory() {
        let (fs, _temp, directory, _) = prepared_script_migration_fixture(false, false, true);
        assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
    }

    #[test]
    fn startup_refuses_only_unfinished_journals_that_write_outside_their_project() {
        for real in [false, true] {
            let (fs, temp, directory, mut journal) =
                prepared_script_migration_fixture(real, false, false);
            let commit = fs.read(directory.join("commit")).unwrap();
            let truth = [(true, false), (false, false), (true, true), (false, true)];
            for (finished, outside) in truth {
                if outside {
                    let root = journal.script.as_ref().unwrap().project.parent().unwrap();
                    let elsewhere = temp.path().join("elsewhere");
                    for entry in &mut journal.entries {
                        if entry.path.starts_with(root) {
                            let name = entry.path.file_name().unwrap().to_owned();
                            entry.path = elsewhere.join(name);
                        }
                    }
                    fs.write(
                        directory.join("intent.json"),
                        serde_json::to_vec(&journal).unwrap(),
                    )
                    .unwrap();
                }
                if finished {
                    fs.write(directory.join("commit"), commit.as_slice())
                        .unwrap();
                } else if fs.metadata(directory.join("commit")).is_ok() {
                    fs.remove_file(directory.join("commit")).unwrap();
                }
                assert_eq!(
                    rolls_back_outside_project(&fs, &directory).unwrap(),
                    !finished && outside,
                    "finished={finished} outside={outside}"
                );
            }
        }
    }

    #[test]
    fn committed_script_migration_recovers_after_later_project_edits() {
        // A committed journal can outlive its commit when retirement fails. Later edits
        // to the project must not turn its cleanup into a startup failure.
        for real in [false, true] {
            let (fs, _temp, directory, journal) =
                prepared_script_migration_fixture(real, false, false);
            let script = journal.script.as_ref().unwrap();
            fs.write(
                script.source.parent().unwrap().join("Later.luau"),
                "return 1",
            )
            .unwrap();
            let project = fs.read(&script.project).unwrap();
            fs.write(&script.project, [project.as_slice(), b"\n"].concat())
                .unwrap();
            assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
        }
    }

    #[test]
    fn script_migration_recovery_nested_preintent_native() {
        let (fs, _temp, directory, _) = prepared_script_migration_fixture(true, false, true);
        assert_eq!(recover(&fs, &directory).unwrap(), Recovery::Committed);
    }

    #[test]
    fn script_migration_recovery_prepared_crlf_migration_phase_boundaries() {
        // These are the actual output, source-transfer, destination-transfer and
        // commit boundaries in execute_replacement_journal. No foreign edit or
        // injected I/O failure is needed for any state below to be legitimate.
        for real in [false, true] {
            for reverse in [false, true] {
                // Three entries produce two outputs, two backups, two installs,
                // then a commit. Boundary zero is a published intent only.
                for boundary in 0..=7 {
                    let (fs, _temp, directory, journal) =
                        prepared_script_migration_fixture(real, reverse, false);
                    let mut operations = Vec::new();
                    for (index, entry) in journal.entries.iter().enumerate() {
                        if entry.desired.is_some() {
                            operations.push(("output", index));
                        }
                    }
                    for (index, entry) in journal.entries.iter().enumerate() {
                        if entry.expected.is_some() {
                            operations.push(("stage", index));
                        }
                    }
                    for (index, entry) in journal.entries.iter().enumerate() {
                        if entry.desired.is_some() {
                            operations.push(("install", index));
                        }
                    }
                    assert_eq!(operations.len(), 6);
                    let mut lock = fs.lock();
                    if boundary < 7 {
                        lock.remove_file(directory.join("commit")).unwrap();
                    }
                    // Rewind the completed production journal, preserving exactly
                    // the bytes that each earlier durable boundary could contain.
                    for (step, (phase, index)) in operations.iter().enumerate().rev() {
                        if step < boundary {
                            continue;
                        }
                        let entry = &journal.entries[*index];
                        let output = directory.join(format!("output-{index}"));
                        let result = match *phase {
                            "install" => transfer(
                                &mut lock,
                                &entry.path,
                                &output,
                                entry.desired.as_ref().unwrap(),
                            ),
                            "stage" => transfer(
                                &mut lock,
                                &stage(&directory, *index),
                                &entry.path,
                                entry.expected.as_ref().unwrap(),
                            ),
                            "output" => lock.remove_file(&output),
                            _ => unreachable!(),
                        };
                        assert!(result.is_ok(), "Script migration rewind real={real} reverse={reverse} boundary={boundary} step={step} phase={phase}. {result:?}. Ordered entries {:#?}", journal.entries);
                    }
                    let actual: Vec<_> = journal
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| {
                            (
                                &entry.path,
                                lock.snapshot_tree(&entry.path).with_not_found().unwrap(),
                                lock.snapshot_tree(&stage(&directory, index))
                                    .with_not_found()
                                    .unwrap(),
                                lock.snapshot_tree(&directory.join(format!("output-{index}")))
                                    .with_not_found()
                                    .unwrap(),
                            )
                        })
                        .collect();
                    let validation =
                        validate_replacement_ownership(&mut lock, &directory, &journal);
                    assert!(validation.is_ok(), "Script migration ownership real={real} reverse={reverse} boundary={boundary}. {validation:?}. Ordered entries {:#?}. Actual path/stage/output {actual:#?}", journal.entries);
                    drop(lock);
                    let result = recover(&fs, &directory);
                    let wanted = if boundary == 7 {
                        Recovery::Committed
                    } else {
                        Recovery::RolledBack
                    };
                    assert!(matches!(&result, Ok(value) if *value == wanted), "Script migration recovery real={real} reverse={reverse} boundary={boundary}. {result:?}, expected {wanted:?}. Ordered operations {operations:?}. Ordered entries {:#?}. Actual path/stage/output {actual:#?}", journal.entries);
                    for entry in &journal.entries {
                        let desired = if boundary == 7 {
                            &entry.desired
                        } else {
                            &entry.expected
                        };
                        assert_eq!(&fs.snapshot_tree(&entry.path).with_not_found().unwrap(), desired,
                            "Script migration terminal real={real} reverse={reverse} boundary={boundary}. Ordered entries {:#?}", journal.entries);
                    }
                    assert_eq!(recover(&fs, &directory).unwrap(), wanted);
                }
            }
        }
    }

    // Keep the original reproduction and its common-root control unchanged. This
    // exercises the same eight durable states with external, absolute mappings.
    fn rewind_script_migration_journal(
        fs: &Vfs,
        directory: &Path,
        journal: &ReplacementJournal,
        boundary: usize,
    ) {
        let mut operations = Vec::new();
        for (index, entry) in journal.entries.iter().enumerate() {
            if entry.desired.is_some() {
                operations.push(("output", index));
            }
        }
        for (index, entry) in journal.entries.iter().enumerate() {
            if entry.expected.is_some() {
                operations.push(("stage", index));
            }
        }
        for (index, entry) in journal.entries.iter().enumerate() {
            if entry.desired.is_some() {
                operations.push(("install", index));
            }
        }
        assert_eq!(operations.len(), 6);
        let mut lock = fs.lock();
        if boundary < 7 {
            lock.remove_file(directory.join("commit")).unwrap();
        }
        for (step, (phase, index)) in operations.iter().enumerate().rev() {
            if step < boundary {
                continue;
            }
            let entry = &journal.entries[*index];
            let output = directory.join(format!("output-{index}"));
            match *phase {
                "install" => transfer(
                    &mut lock,
                    &entry.path,
                    &output,
                    entry.desired.as_ref().unwrap(),
                ),
                "stage" => transfer(
                    &mut lock,
                    &stage(directory, *index),
                    &entry.path,
                    entry.expected.as_ref().unwrap(),
                ),
                "output" => lock.remove_file(&output),
                _ => unreachable!(),
            }
            .unwrap();
        }
    }

    fn restart_migration_filesystem(fs: Vfs, real: bool, root: &Path) -> Vfs {
        let restarted = if real {
            Vfs::new(StdBackend::new().unwrap())
        } else {
            let mut backend = InMemoryFs::new();
            backend
                .load_snapshot(root, fs.snapshot_tree(root).unwrap())
                .unwrap();
            Vfs::new(backend)
        };
        restarted.set_watch_enabled(false);
        restarted
    }

    #[test]
    fn script_migration_recovery_external_phase_boundaries() {
        for real in [false, true] {
            for reverse in [false, true] {
                for boundary in 0..=7 {
                    let (fs, temp, directory, journal) =
                        prepared_script_migration_fixture(real, reverse, true);
                    rewind_script_migration_journal(&fs, &directory, &journal, boundary);
                    let root = dunce::canonicalize(temp.path()).unwrap();
                    let fs = restart_migration_filesystem(fs, real, &root);
                    let wanted = if boundary == 7 {
                        Recovery::Committed
                    } else {
                        Recovery::RolledBack
                    };
                    assert_eq!(
                        recover(&fs, &directory).unwrap(),
                        wanted,
                        "real={real} reverse={reverse} boundary={boundary}"
                    );
                    for entry in &journal.entries {
                        assert_eq!(
                            fs.snapshot_tree(&entry.path).with_not_found().unwrap(),
                            if boundary == 7 {
                                entry.desired.clone()
                            } else {
                                entry.expected.clone()
                            }
                        );
                    }
                    assert_eq!(recover(&fs, &directory).unwrap(), wanted);
                }
            }
        }
    }

    #[test]
    fn script_migration_recovery_unproven_sidecar_identity() {
        for real in [false, true] {
            for nested in [false, true] {
                let (fs, _temp, directory, mut journal) =
                    prepared_script_migration_fixture(real, false, nested);
                let sidecar = journal
                    .script
                    .as_ref()
                    .unwrap()
                    .source
                    .with_file_name("Unrelated.meta.jsonc");
                journal.entries.push(Replacement {
                    path: sidecar,
                    expected: None,
                    desired: Some(VfsSnapshot::file("not this script's metadata")),
                });
                assert!(
                    validate_replacement_ownership(&mut fs.lock(), &directory, &journal).is_err(),
                    "Uncaptured sibling sidecar accepted, real={real} nested={nested}"
                );
            }
        }
    }

    #[test]
    fn script_migration_recovery_external_identity_failures_preserve_preintent_and_recovery() {
        for real in [false, true] {
            for attack in [
                "legacy",
                "read_only",
                "missing_identity",
                "wrong_identity",
                "sidecar",
                "destination",
                "source_pair",
                "cache",
                "project",
                "overlap",
            ] {
                for recovering in [false, true] {
                    let (fs, temp, directory, mut journal) =
                        prepared_script_migration_fixture(real, false, true);
                    rewind_script_migration_journal(&fs, &directory, &journal, 0);
                    let script = journal.script.as_mut().unwrap();
                    let source = script.source.clone();
                    let destination = script.destination.clone();
                    let sidecar = source.with_file_name("LooseLuau.meta.jsonc");
                    match attack {
                        "legacy" => script.migration_ownership = None,
                        "read_only" => {
                            script.migration_ownership.as_mut().unwrap().write_authority = None
                        }
                        "missing_identity" => {
                            script
                                .migration_ownership
                                .as_mut()
                                .unwrap()
                                .paths
                                .remove(&sidecar);
                        }
                        "wrong_identity" => {
                            script
                                .migration_ownership
                                .as_mut()
                                .unwrap()
                                .paths
                                .insert(sidecar.clone(), source.clone());
                        }
                        "sidecar" => {
                            // Even an identity captured by declaration traversal is not
                            // authority to replace an unrelated sibling's metadata.
                            let path = source.with_file_name("Unrelated.meta.jsonc");
                            script
                                .migration_ownership
                                .as_mut()
                                .unwrap()
                                .paths
                                .insert(path.clone(), path.clone());
                            journal.entries.push(Replacement {
                                path,
                                expected: None,
                                desired: Some(VfsSnapshot::file("foreign metadata")),
                            });
                        }
                        "destination" => {
                            let forged = source.with_file_name("Unrelated.client.luau");
                            journal
                                .entries
                                .iter_mut()
                                .find(|entry| entry.path == destination)
                                .unwrap()
                                .path = forged.clone();
                            script.destination = forged;
                        }
                        "source_pair" => {
                            script
                                .migration_ownership
                                .as_mut()
                                .unwrap()
                                .write_authority
                                .as_mut()
                                .unwrap()
                                .source = destination;
                        }
                        "cache" | "project" => {
                            let path = if attack == "cache" {
                                script
                                    .project
                                    .parent()
                                    .unwrap()
                                    .join(".rojo/Unrelated.meta.jsonc")
                            } else {
                                script.project.clone()
                            };
                            journal.entries.push(Replacement {
                                expected: fs.snapshot_tree(&path).with_not_found().unwrap(),
                                desired: Some(VfsSnapshot::file("foreign output")),
                                path,
                            });
                        }
                        "overlap" => journal.entries.push(journal.entries[0].clone()),
                        _ => unreachable!(),
                    }
                    let root = dunce::canonicalize(temp.path()).unwrap();
                    if recovering {
                        fs.write(
                            directory.join("intent.json"),
                            serde_json::to_vec(&journal).unwrap(),
                        )
                        .unwrap();
                        let fs = restart_migration_filesystem(fs, real, &root);
                        let before = fs.snapshot_tree(&root).unwrap();
                        assert!(
                            recover(&fs, &directory).is_err(),
                            "Recovered {attack}, real={real}"
                        );
                        assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
                    } else {
                        let attempt = directory.with_file_name("unpublished");
                        let before = fs.snapshot_tree(&root).unwrap();
                        assert!(
                            execute_replacement_journal(
                                &fs,
                                &attempt,
                                journal,
                                &Default::default()
                            )
                            .is_err(),
                            "Published {attack}, real={real}"
                        );
                        assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
                        assert!(!fs.exists(attempt.join("intent.json")).unwrap());
                    }
                }
            }
        }
    }

    /// Committed journals only acknowledge their commit, so later project changes must
    /// not block them. Unfinished journals must still refuse those changes.
    fn assert_recovery_refused_unless_committed(
        fs: &Vfs,
        directory: &Path,
        committed: bool,
        context: &str,
    ) {
        let result = recover(fs, directory);
        if committed {
            assert_eq!(
                result.unwrap(),
                Recovery::Committed,
                "Refused committed {context}"
            );
        } else {
            assert!(result.is_err(), "Accepted {context}");
        }
    }

    #[test]
    fn script_migration_recovery_external_graph_changes_preserve_every_boundary() {
        for real in [false, true] {
            for boundary in 0..=7 {
                for change in [
                    "root",
                    "nested",
                    "absence",
                    "case",
                    "directory",
                    "wally",
                    "external_wally",
                ] {
                    let (fs, temp, directory, journal) =
                        prepared_script_migration_fixture(real, false, true);
                    rewind_script_migration_journal(&fs, &directory, &journal, boundary);
                    let root = dunce::canonicalize(temp.path()).unwrap();
                    let script = journal.script.as_ref().unwrap();
                    let parent = script.source.parent().unwrap();
                    match change {
                        "root" => fs.write(&script.project, "{}").unwrap(),
                        "nested" => fs.write(parent.join("default.project.json"), "{}").unwrap(),
                        "absence" => {
                            fs.create_dir_all(root.join("missing")).unwrap();
                            fs.write(root.join("missing/optional.project.json"), "{}")
                                .unwrap();
                        }
                        "case" => fs.write(parent.join("DEFAULT.project.json"), "{}").unwrap(),
                        "directory" => fs.create_dir(parent.join("NewMapping")).unwrap(),
                        "wally" => fs
                            .write(root.join("wally.toml"), "[dependencies]\n")
                            .unwrap(),
                        "external_wally" => fs
                            .write(parent.join("wally.toml"), "[dependencies]\n")
                            .unwrap(),
                        _ => unreachable!(),
                    }
                    let fs = restart_migration_filesystem(fs, real, &root);
                    let before = fs.snapshot_tree(&root).unwrap();
                    assert_recovery_refused_unless_committed(
                        &fs,
                        &directory,
                        boundary == 7,
                        &format!("{change}, real={real} boundary={boundary}"),
                    );
                    assert_eq!(
                        fs.snapshot_tree(&root).unwrap(),
                        before,
                        "Mutated {change}, real={real} boundary={boundary}"
                    );
                }
            }
        }
    }

    #[test]
    fn script_migration_recovery_external_ancestor_package_guard() {
        for real in [false, true] {
            for boundary in 0..=7 {
                let (fs, temp, directory, journal) =
                    prepared_script_migration_fixture(real, false, true);
                rewind_script_migration_journal(&fs, &directory, &journal, boundary);
                let root = dunce::canonicalize(temp.path()).unwrap();
                // This ancestor is outside both the journal ancestry and the mapped
                // source directory's membership witness.
                fs.write(root.join("Source/wally.toml"), "[dependencies]\n")
                    .unwrap();
                let fs = restart_migration_filesystem(fs, real, &root);
                let before = fs.snapshot_tree(&root).unwrap();
                assert_recovery_refused_unless_committed(
                    &fs,
                    &directory,
                    boundary == 7,
                    &format!("external package, real={real} boundary={boundary}"),
                );
                assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn script_migration_recovery_external_canonical_ancestor_substitution_is_read_only() {
        for boundary in 0..=7 {
            let (fs, temp, directory, journal) =
                prepared_script_migration_fixture(true, false, true);
            rewind_script_migration_journal(&fs, &directory, &journal, boundary);
            let root = dunce::canonicalize(temp.path()).unwrap();
            let parent = journal.script.as_ref().unwrap().source.parent().unwrap();
            let moved = root.join("MovedSources");
            std::fs::rename(parent, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, parent).unwrap();
            let before = fs.snapshot_tree(&moved).unwrap();
            let backup = fs.snapshot_tree(&directory).unwrap();
            let fs = restart_migration_filesystem(fs, true, &root);
            assert_recovery_refused_unless_committed(
                &fs,
                &directory,
                boundary == 7,
                &format!("substituted ancestor, boundary={boundary}"),
            );
            assert_eq!(fs.snapshot_tree(&moved).unwrap(), before);
            assert_eq!(fs.snapshot_tree(&directory).unwrap(), backup);
        }
    }

    fn script_replacement_interrupted(
        real: bool,
        phase: usize,
    ) -> (Vfs, tempfile::TempDir, PathBuf, PathBuf, ReplacementJournal) {
        script_replacement_interrupted_with_optional_guard(real, phase, false)
    }

    fn script_replacement_interrupted_with_optional_guard(
        real: bool,
        phase: usize,
        optional_guard: bool,
    ) -> (Vfs, tempfile::TempDir, PathBuf, PathBuf, ReplacementJournal) {
        let (fs, temp, root) = fixture(real);
        let project_path = root.join("default.project.json");
        let project_bytes: &[u8] = if optional_guard {
            br#"{"name":"Test","tree":{"$className":"Folder","$path":"src","Optional":{"$path":{"optional":"missing/nested.project.json"}}}}"#
        } else {
            br#"{"name":"Test","tree":{"$className":"Folder","$path":"src"}}"#
        };
        let nested_bytes = br#"{"name":"Nested","tree":{"$className":"Folder"}}"#;
        fs.write(&project_path, project_bytes).unwrap();
        fs.write(root.join("src/default.project.json"), nested_bytes)
            .unwrap();
        fs.write(root.join("src/import.luau"), "import input")
            .unwrap();
        let project = crate::Project::load_exact(&fs, &project_path, None).unwrap();
        let directory = root.join(".rojo/relocations/script");
        fs.create_dir_all(directory.parent().unwrap()).unwrap();
        let entries = vec![
            Replacement {
                path: root.join("src/Code.legacy.lua"),
                expected: Some(VfsSnapshot::file("old source")),
                desired: None,
            },
            Replacement {
                path: root.join("src/Code.client.lua"),
                expected: None,
                desired: Some(VfsSnapshot::file("new source")),
            },
            Replacement {
                path: root.join("src/Code.meta.jsonc"),
                expected: Some(VfsSnapshot::file("old meta")),
                desired: Some(VfsSnapshot::file("new meta")),
            },
        ];
        for entry in &entries {
            if let Some(expected) = &entry.expected {
                materialize(&mut fs.lock(), &entry.path, expected).unwrap();
            }
        }
        let mut expected = std::collections::HashMap::from([
            (project_path, Some(project_bytes.to_vec())),
            (
                root.join("src/default.project.json"),
                Some(nested_bytes.to_vec()),
            ),
            (root.join("src/default.project.jsonc"), None),
            (root.join("src/import.luau"), Some(b"import input".to_vec())),
            (
                root.join("src/Code.legacy.lua"),
                Some(b"old source".to_vec()),
            ),
            (root.join("src/Code.client.lua"), None),
            (root.join("src/Code.meta.jsonc"), Some(b"old meta".to_vec())),
        ]);
        let ownership = optional_guard.then(|| {
            expected.insert(root.join("missing/nested.project.json"), None);
            let mut ownership = super::super::ScriptMigrationOwnership::default();
            for (path, bytes) in &expected {
                if crate::Project::is_project_file(path) {
                    assert_eq!(ownership.capture(&fs, path).unwrap(), *bytes);
                }
            }
            ownership
        });
        execute_script_replacements_guarded(
            &fs,
            &directory,
            &project,
            entries,
            &expected,
            ownership.as_ref(),
        )
        .unwrap();
        let journal: ReplacementJournal =
            serde_json::from_slice(&fs.read(directory.join("intent.json")).unwrap()).unwrap();
        // Reconstruct each durable boundary from a completed transaction, retaining
        // exactly the originals and generated outputs that exist at that boundary.
        {
            let mut lock = fs.lock();
            if phase < 7 {
                lock.remove_file(directory.join("commit")).unwrap();
            }
            for (threshold, index) in [(6, 2), (5, 1)] {
                if phase < threshold {
                    transfer(
                        &mut lock,
                        &journal.entries[index].path,
                        &directory.join(format!("output-{index}")),
                        journal.entries[index].desired.as_ref().unwrap(),
                    )
                    .unwrap();
                }
            }
            for (threshold, index) in [(4, 2), (3, 0)] {
                if phase < threshold {
                    transfer(
                        &mut lock,
                        &stage(&directory, index),
                        &journal.entries[index].path,
                        journal.entries[index].expected.as_ref().unwrap(),
                    )
                    .unwrap();
                }
            }
            for (threshold, index) in [(2, 2), (1, 1)] {
                if phase < threshold {
                    lock.remove_file(directory.join(format!("output-{index}")))
                        .unwrap();
                }
            }
        }
        (fs, temp, root, directory, journal)
    }

    #[test]
    fn script_replacement_optional_missing_parent_migrates_and_recovers() {
        for real in [false, true] {
            for (phase, unicode_sibling) in
                (0..8).flat_map(|phase| [false, true].map(move |unicode| (phase, unicode)))
            {
                let (fs, _temp, root, directory, journal) =
                    script_replacement_interrupted_with_optional_guard(real, phase, true);
                let sibling = root.join("uncertain-\u{e9}");
                if unicode_sibling {
                    fs.create_dir(&sibling).unwrap();
                    validate_script_project_guards(
                        &mut fs.lock(),
                        &directory,
                        &journal,
                        journal.script.as_ref().unwrap(),
                    )
                    .unwrap();
                }
                let restarted = if real {
                    Vfs::new(StdBackend::new().unwrap())
                } else {
                    fs
                };
                restarted.set_watch_enabled(false);
                let outcome = if phase == 7 {
                    Recovery::Committed
                } else {
                    Recovery::RolledBack
                };
                assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
                assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
                assert_eq!(restarted.exists(&sibling).unwrap(), unicode_sibling);
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
                assert!(restarted
                    .metadata(root.join("missing"))
                    .with_not_found()
                    .unwrap()
                    .is_none());
            }
        }
    }

    #[test]
    fn script_replacement_optional_missing_parent_conflicts_retain_journal() {
        for real in [false, true] {
            for phase in [0, 5, 7] {
                for change in [
                    "appearance",
                    "ancestor file",
                    "case alias",
                    "missing identity",
                    "changed identity",
                    "missing proof",
                ] {
                    let (fs, _temp, root, directory, mut journal) =
                        script_replacement_interrupted_with_optional_guard(real, phase, true);
                    match change {
                        "appearance" => {
                            fs.create_dir(root.join("missing")).unwrap();
                            fs.write(root.join("missing/nested.project.json"), "foreign")
                                .unwrap();
                        }
                        "ancestor file" => fs.write(root.join("missing"), "foreign").unwrap(),
                        "case alias" => fs.create_dir(root.join("MISSING")).unwrap(),
                        "missing identity" | "changed identity" | "missing proof" => {
                            let script = journal.script.as_mut().unwrap();
                            if change == "missing proof" {
                                script.migration_ownership = None;
                            } else {
                                let paths = &mut script.migration_ownership.as_mut().unwrap().paths;
                                let guard = root.join("missing/nested.project.json");
                                if change == "missing identity" {
                                    paths.remove(&guard);
                                } else {
                                    paths.insert(guard, root.join("other/nested.project.json"));
                                }
                            }
                            fs.write(
                                directory.join("intent.json"),
                                serde_json::to_vec(&journal).unwrap(),
                            )
                            .unwrap();
                        }
                        _ => unreachable!(),
                    }
                    // The same proof must fail at the live commit boundary, not
                    // only when the serialized intent is read after a restart.
                    assert!(validate_script_project_guards(
                        &mut fs.lock(),
                        &directory,
                        &journal,
                        journal.script.as_ref().unwrap(),
                    )
                    .is_err());
                    let restarted = if real {
                        Vfs::new(StdBackend::new().unwrap())
                    } else {
                        fs
                    };
                    restarted.set_watch_enabled(false);
                    let before = restarted.snapshot_tree(&root).unwrap();
                    assert_recovery_refused_unless_committed(
                        &restarted,
                        &directory,
                        phase == 7,
                        &format!("{change}, phase {phase}, real {real}"),
                    );
                    assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn script_replacement_optional_missing_parent_rejects_symlink_substitutions() {
        for phase in [0, 5, 7] {
            for link in [
                "dangling ancestor",
                "resolved ancestor",
                "dangling leaf",
                "existing ancestor",
                "unresolved Unicode sibling",
            ] {
                let (fs, _temp, root, directory, journal) =
                    script_replacement_interrupted_with_optional_guard(true, phase, true);
                let outside = tempfile::tempdir().unwrap();
                let outside = dunce::canonicalize(outside.path()).unwrap();
                let (path, target) = match link {
                    "dangling ancestor" => (root.join("missing"), outside.join("absent")),
                    "resolved ancestor" => (root.join("missing"), outside.clone()),
                    "unresolved Unicode sibling" => {
                        (root.join("uncertain-\u{e9}"), outside.join("absent"))
                    }
                    "dangling leaf" => {
                        fs.create_dir(root.join("missing")).unwrap();
                        (
                            root.join("missing/nested.project.json"),
                            outside.join("absent"),
                        )
                    }
                    "existing ancestor" => {
                        fs.create_dir(root.join("guards")).unwrap();
                        let path = root.join("guards/deep/nested.project.json");
                        let mut journal: ReplacementJournal = serde_json::from_slice(
                            &fs.read(directory.join("intent.json")).unwrap(),
                        )
                        .unwrap();
                        let script = journal.script.as_mut().unwrap();
                        script.project_guards.push((path.clone(), None));
                        script
                            .migration_ownership
                            .as_mut()
                            .unwrap()
                            .capture(&fs, &path)
                            .unwrap();
                        fs.write(
                            directory.join("intent.json"),
                            serde_json::to_vec(&journal).unwrap(),
                        )
                        .unwrap();
                        std::fs::rename(root.join("guards"), outside.join("original")).unwrap();
                        (root.join("guards"), outside.join("original"))
                    }
                    _ => unreachable!(),
                };
                std::os::unix::fs::symlink(&target, &path).unwrap();
                let restarted = Vfs::new(StdBackend::new().unwrap());
                restarted.set_watch_enabled(false);
                let journal_before = restarted.snapshot_tree(&directory).unwrap();
                let outside_before = restarted.snapshot_tree(&outside).unwrap();
                let entries_before: Vec<_> = journal
                    .entries
                    .iter()
                    .map(|entry| {
                        restarted
                            .snapshot_tree(&entry.path)
                            .with_not_found()
                            .unwrap()
                    })
                    .collect();
                assert_recovery_refused_unless_committed(
                    &restarted,
                    &directory,
                    phase == 7,
                    &format!("{link}, phase {phase}"),
                );
                assert_eq!(restarted.snapshot_tree(&directory).unwrap(), journal_before);
                assert_eq!(restarted.snapshot_tree(&outside).unwrap(), outside_before);
                for (entry, before) in journal.entries.iter().zip(entries_before) {
                    assert_eq!(
                        restarted
                            .snapshot_tree(&entry.path)
                            .with_not_found()
                            .unwrap(),
                        before
                    );
                }
                assert_eq!(std::fs::read_link(&path).unwrap(), target);
                std::fs::remove_file(&path).unwrap();
                if link == "existing ancestor" {
                    std::fs::rename(&target, &path).unwrap();
                }
                let outcome = if phase == 7 {
                    Recovery::Committed
                } else {
                    Recovery::RolledBack
                };
                assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
                assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
            }
        }
    }

    #[test]
    fn script_replacement_optional_missing_parent_does_not_relax_write_parents() {
        for real in [false, true] {
            let (fs, _temp, root) = fixture(real);
            for check_parents in [false, true] {
                for path in [
                    root.join("missing/Code.client.luau"),
                    root.join("missing/journal"),
                ] {
                    let error = validate_paths(&mut fs.lock(), &[path.as_path()], check_parents)
                        .unwrap_err();
                    assert!(error
                        .to_string()
                        .contains("Recovery cannot validate the parent"));
                }
            }
            let (fs, _temp, root, directory, mut journal) =
                script_replacement_interrupted_with_optional_guard(real, 5, true);
            let before = fs.snapshot_tree(&root).unwrap();
            for index in 0..journal.entries.len() {
                let original = journal.entries[index].path.clone();
                journal.entries[index].path =
                    root.join("missing").join(original.file_name().unwrap());
                let error = validate_script_project_guards(
                    &mut fs.lock(),
                    &directory,
                    &journal,
                    journal.script.as_ref().unwrap(),
                )
                .unwrap_err();
                assert!(error
                    .to_string()
                    .contains("Recovery cannot validate the parent"));
                journal.entries[index].path = original;
            }
            let error = validate_script_project_guards(
                &mut fs.lock(),
                &root.join("missing/journal"),
                &journal,
                journal.script.as_ref().unwrap(),
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("Recovery cannot validate the parent"));
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[test]
    fn script_replacement_restart_accepts_backend_separator_spellings() {
        for real in [false, true] {
            for optional_guard in [false, true] {
                for component_spelling in [false, true] {
                    let (fs, _temp, _root, directory, mut journal) =
                        script_replacement_interrupted_with_optional_guard(real, 0, optional_guard);
                    if component_spelling {
                        for (path, _) in &mut journal.script.as_mut().unwrap().project_guards {
                            *path = path.components().collect();
                        }
                    }
                    fs.write(
                        directory.join("intent.json"),
                        serde_json::to_vec(&journal).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
                    for entry in &journal.entries {
                        assert_eq!(
                            fs.snapshot_tree(&entry.path).with_not_found().unwrap(),
                            entry.expected
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn script_replacement_restart_preserves_nested_declaration_conflicts_at_every_boundary() {
        let mut accepted = Vec::new();
        for real in [false, true] {
            for phase in 0..8 {
                for change in ["unchanged", "changed", "removed", "created"] {
                    let (fs, _temp, root, directory, journal) =
                        script_replacement_interrupted(real, phase);
                    let nested = root.join("src/default.project.json");
                    let absent = root.join("src/default.project.jsonc");
                    let original = fs.read(&nested).unwrap();
                    match change {
                        "changed" => fs
                            .write(&nested, "{\"name\":\"Foreign\",\"tree\":{}}")
                            .unwrap(),
                        "removed" => fs.remove_file(&nested).unwrap(),
                        "created" => fs
                            .write(&absent, "{\"name\":\"Foreign\",\"tree\":{}}")
                            .unwrap(),
                        _ => {}
                    }
                    let restarted = if real {
                        Vfs::new(StdBackend::new().unwrap())
                    } else {
                        fs
                    };
                    restarted.set_watch_enabled(false);
                    if change != "unchanged" {
                        let before = restarted.snapshot_tree(&root).unwrap();
                        if phase == 7 {
                            // A committed journal is only acknowledged, never replayed.
                            assert_recovery_refused_unless_committed(
                                &restarted,
                                &directory,
                                true,
                                &format!("{change}, phase {phase}, real {real}"),
                            );
                            assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
                            continue;
                        }
                        if recover(&restarted, &directory).is_ok() {
                            accepted.push(format!("{change}, phase {phase}, real {real}"));
                            continue;
                        }
                        assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
                        if change == "created" {
                            restarted.remove_file(&absent).unwrap();
                        } else {
                            restarted.write(&nested, original.as_slice()).unwrap();
                        }
                    }
                    let outcome = if phase == 7 {
                        Recovery::Committed
                    } else {
                        Recovery::RolledBack
                    };
                    assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
                    assert_eq!(recover(&restarted, &directory).unwrap(), outcome);
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
                    assert_eq!(restarted.read(&nested).unwrap(), original);
                    assert!(restarted.read(&absent).with_not_found().unwrap().is_none());
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "Recovery accepted conflicting declarations {accepted:?}"
        );
    }

    #[test]
    fn script_replacement_persists_only_frozen_project_expectations() {
        let (fs, _temp, root, directory, journal) = script_replacement_interrupted(false, 5);
        assert_eq!(
            journal.script.as_ref().unwrap().project_guards,
            vec![
                (
                    root.join("default.project.json"),
                    Some(
                        fs.read(root.join("default.project.json"))
                            .unwrap()
                            .as_ref()
                            .clone()
                    )
                ),
                (
                    root.join("src/default.project.json"),
                    Some(
                        fs.read(root.join("src/default.project.json"))
                            .unwrap()
                            .as_ref()
                            .clone()
                    )
                ),
                (root.join("src/default.project.jsonc"), None),
            ]
        );
        // Non-declaration import inputs remain ephemeral, rather than acquiring
        // unrelated recovery authority through this narrowly scoped evidence.
        fs.write(root.join("src/import.luau"), "later import edit")
            .unwrap();
        assert_eq!(recover(&fs, &directory).unwrap(), Recovery::RolledBack);
        let project_path = root.join("default.project.json");
        let project = crate::Project::load_exact(&fs, &project_path, None).unwrap();
        let frozen = std::collections::HashMap::from([
            (
                project_path.clone(),
                Some(fs.read(&project_path).unwrap().as_ref().clone()),
            ),
            (
                root.join("src/default.project.json"),
                Some(
                    fs.read(root.join("src/default.project.json"))
                        .unwrap()
                        .as_ref()
                        .clone(),
                ),
            ),
            (root.join("src/default.project.jsonc"), None),
        ]);
        for state in [
            "missing root",
            "absent root",
            "edited root",
            "edited nested",
            "created probe",
        ] {
            let mut expected = frozen.clone();
            match state {
                "missing root" => {
                    expected.remove(&project_path);
                }
                "absent root" => {
                    expected.insert(project_path.clone(), None);
                }
                "edited root" => fs.write(&project_path, "{}").unwrap(),
                "edited nested" => fs
                    .write(root.join("src/default.project.json"), "{}")
                    .unwrap(),
                "created probe" => fs
                    .write(root.join("src/default.project.jsonc"), "{}")
                    .unwrap(),
                _ => unreachable!(),
            }
            let before = fs.snapshot_tree(&root).unwrap();
            assert!(
                execute_script_replacements(
                    &fs,
                    &root.join(".rojo/relocations/stale"),
                    &project,
                    journal.entries.clone(),
                    &expected,
                )
                .is_err(),
                "Accepted {state} expectation"
            );
            assert_eq!(fs.snapshot_tree(&root).unwrap(), before);
            for (path, bytes) in &frozen {
                if let Some(bytes) = bytes {
                    fs.write(path, bytes).unwrap();
                } else if fs.metadata(path).with_not_found().unwrap().is_some() {
                    fs.remove_file(path).unwrap();
                }
            }
        }
    }

    #[test]
    fn script_replacement_restart_checks_external_guards_without_granting_write_authority() {
        for change in [
            "unchanged",
            "changed",
            "removed",
            "created",
            "foreign write",
        ] {
            let (fs, _temp, root, directory, journal) = script_replacement_interrupted(true, 5);
            let outside = tempfile::tempdir().unwrap();
            let outside_root = dunce::canonicalize(outside.path()).unwrap();
            let project = outside_root.join("nested.project.json");
            let absent = outside_root.join("default.project.jsonc");
            let bytes = b"external project declaration";
            fs.write(&project, bytes).unwrap();
            let intent_path = directory.join("intent.json");
            let mut intent = serde_json::to_value(&journal).unwrap();
            let guards = intent["script"]["project_guards"].as_array_mut().unwrap();
            guards.push(serde_json::json!([project, bytes.as_slice()]));
            guards.push(serde_json::json!([absent, null]));
            match change {
                "changed" => fs.write(&project, "foreign declaration").unwrap(),
                "removed" => fs.remove_file(&project).unwrap(),
                "created" => fs.write(&absent, "foreign declaration").unwrap(),
                "foreign write" => {
                    let foreign_source = outside_root.join("Code.legacy.lua");
                    let foreign_destination = outside_root.join("Code.client.lua");
                    intent["script"]["source"] = serde_json::json!(foreign_source);
                    intent["script"]["destination"] = serde_json::json!(foreign_destination);
                    intent["entries"][0]["path"] = serde_json::json!(foreign_source);
                    intent["entries"][1]["path"] = serde_json::json!(foreign_destination);
                    fs.write(&foreign_destination, "new source").unwrap();
                    intent["entries"].as_array_mut().unwrap().truncate(2);
                }
                _ => {}
            }
            fs.write(&intent_path, serde_json::to_vec(&intent).unwrap())
                .unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            if change != "unchanged" {
                let before = restarted.snapshot_tree(&root).unwrap();
                let foreign = restarted.snapshot_tree(&outside_root).unwrap();
                assert!(
                    recover(&restarted, &directory).is_err(),
                    "Accepted {change}"
                );
                assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
                assert_eq!(restarted.snapshot_tree(&outside_root).unwrap(), foreign);
                if change == "foreign write" {
                    continue;
                }
                if change == "created" {
                    restarted.remove_file(&absent).unwrap();
                } else {
                    restarted.write(&project, bytes).unwrap();
                }
            }
            let foreign = restarted.snapshot_tree(&outside_root).unwrap();
            assert_eq!(
                recover(&restarted, &directory).unwrap(),
                Recovery::RolledBack
            );
            assert_eq!(restarted.snapshot_tree(&outside_root).unwrap(), foreign);
        }
    }

    #[test]
    fn script_replacement_restart_refuses_missing_declaration_evidence() {
        for packages in [false, true] {
            let (fs, _temp, root, directory, _) = script_replacement_interrupted(true, 5);
            if packages {
                package_fixture(&fs, &root);
            }
            let intent_path = directory.join("intent.json");
            let mut intent: serde_json::Value =
                serde_json::from_slice(&fs.read(&intent_path).unwrap()).unwrap();
            intent["script"]
                .as_object_mut()
                .unwrap()
                .remove("project_guards");
            if packages {
                intent["ownership"] =
                    serde_json::to_value(ReplacementOwnership::capture(&fs, &directory).unwrap())
                        .unwrap();
            }
            fs.write(&intent_path, serde_json::to_vec(&intent).unwrap())
                .unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            let before = restarted.snapshot_tree(&root).unwrap();
            assert!(recover(&restarted, &directory).is_err());
            assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn script_replacement_restart_refuses_noncanonical_declaration_guards() {
        for leaf in [false, true] {
            let (fs, _temp, root, directory, _) = script_replacement_interrupted(true, 5);
            let outside = tempfile::tempdir().unwrap();
            let outside_root = dunce::canonicalize(outside.path()).unwrap();
            let declaration = if leaf {
                root.join("src/default.project.json")
            } else {
                root.join("dst/nested.project.json")
            };
            if !leaf {
                fs.write(&declaration, "read-only declaration").unwrap();
                let intent_path = directory.join("intent.json");
                let mut intent: serde_json::Value =
                    serde_json::from_slice(&fs.read(&intent_path).unwrap()).unwrap();
                intent["script"]["project_guards"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!([
                        declaration,
                        b"read-only declaration".as_slice()
                    ]));
                fs.write(&intent_path, serde_json::to_vec(&intent).unwrap())
                    .unwrap();
            }
            let original = if leaf {
                declaration.clone()
            } else {
                root.join("dst")
            };
            let moved = outside_root.join("original");
            let before = fs.snapshot_tree(&root).unwrap();
            std::fs::rename(&original, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &original).unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            let foreign = restarted.snapshot_tree(&outside_root).unwrap();
            assert!(recover(&restarted, &directory).is_err());
            assert_eq!(std::fs::read_link(&original).unwrap(), moved);
            assert_eq!(restarted.snapshot_tree(&outside_root).unwrap(), foreign);
            std::fs::remove_file(&original).unwrap();
            std::fs::rename(&moved, &original).unwrap();
            assert_eq!(restarted.snapshot_tree(&root).unwrap(), before);
            assert_eq!(
                recover(&restarted, &directory).unwrap(),
                Recovery::RolledBack
            );
        }
    }

    #[test]
    fn script_replacement_restart_refuses_invalid_declaration_evidence_without_mutation() {
        for case in [
            "null",
            "empty",
            "missing root",
            "absent root",
            "different root",
            "relative",
            "traversal",
            "dot",
            "nonproject",
            "journal internal",
            "entry overlap",
            "duplicate",
            "nonfile bytes",
            "directory",
            "directory witness",
        ] {
            let (fs, _temp, root, directory, _) = script_replacement_interrupted(true, 5);
            let project = root.join("default.project.json");
            let nested = root.join("src/default.project.json");
            let mut guards = serde_json::json!([
                [project, fs.read(&project).unwrap().as_slice()],
                [nested, fs.read(&nested).unwrap().as_slice()],
                [root.join("src/default.project.jsonc"), null],
            ]);
            match case {
                "null" => guards = serde_json::Value::Null,
                "empty" => guards = serde_json::json!([]),
                "missing root" => {
                    guards.as_array_mut().unwrap().remove(0);
                }
                "absent root" => guards[0][1] = serde_json::Value::Null,
                "different root" => guards[0][1] = serde_json::json!(b"different".as_slice()),
                "relative" => guards[1][0] = serde_json::json!("src/default.project.json"),
                "traversal" => {
                    guards[1][0] = serde_json::json!(root.join("src/../src/default.project.json"))
                }
                "dot" => guards[1][0] = serde_json::json!(root.join("src/./default.project.json")),
                "nonproject" => guards[1][0] = serde_json::json!(root.join("src/import.luau")),
                "journal internal" => {
                    guards[1][0] = serde_json::json!(directory.join("nested.project.json"))
                }
                "entry overlap" => {
                    guards[1][0] =
                        serde_json::json!(root.join("src/Code.client.lua/default.project.json"))
                }
                "duplicate" => {
                    let duplicate = guards[0].clone();
                    guards.as_array_mut().unwrap().push(duplicate);
                }
                "nonfile bytes" => guards[1][1] = serde_json::json!({"Dir": {"children": {}}}),
                "directory" => {
                    fs.remove_file(&nested).unwrap();
                    fs.create_dir(&nested).unwrap();
                }
                _ => {}
            }
            let intent_path = directory.join("intent.json");
            let mut intent: serde_json::Value =
                serde_json::from_slice(&fs.read(&intent_path).unwrap()).unwrap();
            intent["script"]["project_guards"] = guards;
            if case == "directory witness" {
                intent["script"]["expected"] = serde_json::to_value(VfsSnapshot::Dir {
                    children: Default::default(),
                })
                .unwrap();
            }
            fs.write(&intent_path, serde_json::to_vec(&intent).unwrap())
                .unwrap();
            let restarted = Vfs::new(StdBackend::new().unwrap());
            restarted.set_watch_enabled(false);
            let before = restarted.snapshot_tree(&root).unwrap();
            assert!(
                recover(&restarted, &directory).is_err(),
                "Accepted {case} evidence"
            );
            assert_eq!(
                restarted.snapshot_tree(&root).unwrap(),
                before,
                "Modified {case} evidence"
            );
        }
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
                    script: None,
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
