//! Writes the place a live edit opens in Studio, straight from the served tree.
//!
//! The Rojo+ plugin connects with no UI to the address stored in the Workspace
//! attribute below. Writing the place from the tree `rojo serve` already loaded
//! saves a second project load and a separate marking pass.

use std::{
    fs::{File, TryLockError},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use rbx_dom_weak::{
    types::{Attributes, Variant},
    ustr,
};

use crate::{serve_session::SyncDirection, session_id::SessionId, snapshot::RojoTree};

use super::{build::OutputKind, display, export::write_output};

/// Read by the plugin's `App:readLiveEditMarker`, formatted as `host:port`.
pub(super) const MARKER_ATTRIBUTE: &str = "__RojoPlus_LiveEditUrl";
const SESSION_ATTRIBUTE: &str = "__RojoPlus_LiveEditSessionId";
const DIRECTION_ATTRIBUTE: &str = "__RojoPlus_LiveEditDirection";
const BOOTSTRAP_ATTRIBUTE: &str = "__RojoPlus_LiveEditBootstrapped";
const PORT_PLACEHOLDER: &str = "{port}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveEditPlace {
    template: String,
    kind: OutputKind,
}

impl LiveEditPlace {
    /// Validates a path template before the project loads, so a bad flag fails
    /// fast. The port separates most servers; output ownership rejects collisions.
    pub(super) fn parse(template: &Path) -> anyhow::Result<Self> {
        let shown = display::display_path(
            &template.to_string_lossy(),
            display::home_dir().as_deref(),
            false,
        );
        let Some(text) = template.to_str() else {
            bail!("The live edit place path \"{shown}\" is not valid UTF-8.");
        };
        if !template
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(PORT_PLACEHOLDER))
        {
            bail!(
                "The live edit place path \"{shown}\" must contain \"{PORT_PLACEHOLDER}\", which \
                 is replaced by the server's port in the filename."
            );
        }
        let kind = match OutputKind::from_output_path(template) {
            Some(kind @ (OutputKind::Rbxl | OutputKind::Rbxlx)) => kind,
            _ => bail!("The live edit place path \"{shown}\" must end in .rbxl or .rbxlx."),
        };
        Ok(Self {
            template: text.to_owned(),
            kind,
        })
    }

    pub(super) fn resolve(&self, port: u16) -> PathBuf {
        let path = Path::new(&self.template);
        let filename = path.file_name().unwrap().to_str().unwrap();
        path.with_file_name(filename.replace(PORT_PLACEHOLDER, &port.to_string()))
    }

    pub(super) fn kind(&self) -> OutputKind {
        self.kind
    }
}

/// Owns an output until the server stops and its place writer has joined.
/// The OS releases the lock on drop or process exit, including crashes.
#[derive(Debug)]
#[must_use]
pub(super) struct OutputOwnership {
    _lock: File,
}

impl OutputOwnership {
    pub(super) fn acquire(path: &Path) -> anyhow::Result<Self> {
        let shown = display::display_path(
            &path.to_string_lossy(),
            display::home_dir().as_deref(),
            false,
        );
        let mut lock_path = path.as_os_str().to_owned();
        // Studio reserves "<place>.lock" for its own editable-place ownership.
        lock_path.push(".rojo-live-edit.lock");
        // Never truncate, replace, or unlink the sidecar, even after unlocking. All
        // contenders must lock the same inode across output replacement and takeover.
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| {
                format!("Could not open the ownership lock for live edit place \"{shown}\".")
            })?;
        match lock.try_lock() {
            Ok(()) => Ok(Self { _lock: lock }),
            Err(TryLockError::WouldBlock) => bail!(
                "The live edit place \"{shown}\" is already owned by another server. \
                 Choose a different --live-edit-place template or stop that server."
            ),
            Err(TryLockError::Error(error)) => Err(error)
                .with_context(|| format!("Could not lock the live edit place \"{shown}\".")),
        }
    }
}

/// A connection marker preserves the actual bind address, not its presentation label.
/// Wildcard listeners are reached through the loopback address of the same family.
pub(super) fn connection_address(address: SocketAddr) -> String {
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, address.port()).to_string()
}

/// Deletes a place left over from an earlier server on the same port. A consumer
/// waits for the file to appear, so an old copy would open with a stale tree.
/// The caller must hold output ownership before removing it.
pub(super) fn remove_stale(path: &Path) -> anyhow::Result<()> {
    match fs_err::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not remove an old live edit place."),
    }
}

/// Writes the tree as a place whose Workspace carries `marker`. The tree itself
/// never keeps the attribute, so clients syncing from it never see the marker.
/// The file appears all at once, which lets a consumer simply wait for it.
pub(super) fn write(
    tree: &mut RojoTree,
    path: &Path,
    kind: OutputKind,
    marker: &str,
    policy: Option<(SessionId, SyncDirection)>,
) -> anyhow::Result<()> {
    let root = tree.get_root_id();
    let workspace = tree
        .get_instance(root)
        .unwrap()
        .children()
        .iter()
        .copied()
        .find(|&id| tree.get_instance(id).unwrap().class_name() == "Workspace")
        .context("The served project has no Workspace to carry the live edit marker.")?;

    let key = ustr("Attributes");
    let mut attributes = match tree.get_instance(workspace).unwrap().properties().get(&key) {
        Some(Variant::Attributes(attributes)) => attributes.clone(),
        _ => Attributes::new(),
    };
    attributes.insert(
        MARKER_ATTRIBUTE.to_owned(),
        Variant::String(marker.to_owned()),
    );
    for key in [SESSION_ATTRIBUTE, DIRECTION_ATTRIBUTE, BOOTSTRAP_ATTRIBUTE] {
        attributes.remove(key);
    }
    if let Some((session_id, direction)) = policy {
        attributes.insert(
            SESSION_ATTRIBUTE.to_owned(),
            Variant::String(session_id.to_string()),
        );
        attributes.insert(
            DIRECTION_ATTRIBUTE.to_owned(),
            Variant::String(direction.as_str().to_owned()),
        );
    }

    let mut bytes = Vec::new();
    tree.with_property_override(workspace, key, Variant::Attributes(attributes), |tree| {
        super::build::write_tree(tree, &mut bytes, kind)
    })
    .unwrap()?;

    write_output(path, &bytes).with_context(|| {
        let shown = display::display_path(
            &path.to_string_lossy(),
            display::home_dir().as_deref(),
            false,
        );
        format!("Could not write the live edit place \"{shown}\".")
    })
}

#[cfg(test)]
mod tests {
    use rbx_dom_weak::UstrMap;

    use super::*;
    use crate::snapshot::InstanceSnapshot;

    fn tree() -> RojoTree {
        RojoTree::new(
            InstanceSnapshot::new()
                .name("Game")
                .class_name("DataModel")
                .children(vec![
                    InstanceSnapshot::new()
                        .name("Workspace")
                        .class_name("Workspace")
                        .properties(UstrMap::from_iter([(
                            ustr("Attributes"),
                            Variant::Attributes(
                                Attributes::new().with(
                                    "Existing".to_owned(),
                                    Variant::String("kept".to_owned()),
                                ),
                            ),
                        )])),
                    InstanceSnapshot::new()
                        .name("ReplicatedStorage")
                        .class_name("ReplicatedStorage"),
                ]),
        )
    }

    fn workspace_attributes(dom: &rbx_dom_weak::WeakDom) -> Attributes {
        let workspace = dom
            .descendants()
            .find(|instance| instance.class == "Workspace")
            .unwrap();
        match workspace.properties.get(&ustr("Attributes")) {
            Some(Variant::Attributes(attributes)) => attributes.clone(),
            _ => Attributes::new(),
        }
    }

    #[test]
    fn output_ownership_preserves_an_existing_studio_sidecar_lock() {
        for filename in ["Live.34872.rbxl", "Live.34872.rbxlx"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(filename);
            let studio_path = directory.path().join(format!("{filename}.lock"));
            std::fs::write(&path, b"an active place").unwrap();
            std::fs::write(&studio_path, b"Studio owns this place").unwrap();
            let mut studio = File::options()
                .read(true)
                .write(true)
                .open(&studio_path)
                .unwrap();
            studio.try_lock().unwrap();

            let owner = OutputOwnership::acquire(&path).unwrap();
            let mut studio_contents = Vec::new();
            std::io::Read::read_to_end(&mut studio, &mut studio_contents).unwrap();
            assert_eq!(studio_contents, b"Studio owns this place");
            assert_eq!(std::fs::read(&path).unwrap(), b"an active place");
            drop(owner);
            let contender = File::options()
                .read(true)
                .write(true)
                .open(&studio_path)
                .unwrap();
            assert!(matches!(
                contender.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
        }
    }

    #[test]
    fn output_ownership_leaves_studios_sidecar_available() {
        for filename in ["Live.34872.rbxl", "Live.34872.rbxlx"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(filename);
            let studio_path = directory.path().join(format!("{filename}.lock"));
            let _owner = OutputOwnership::acquire(&path).unwrap();

            let studio = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&studio_path)
                .unwrap();
            studio.try_lock().unwrap();
            assert!(OutputOwnership::acquire(&path).is_err());
        }
    }

    #[test]
    fn output_ownership_refuses_a_second_owner_without_touching_the_place() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        std::fs::write(&path, b"an active place").unwrap();
        let _owner = OutputOwnership::acquire(&path).unwrap();

        let error = OutputOwnership::acquire(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains("already owned by another server"));
        assert!(error.to_string().contains("Live.34872.rbxl"));
        assert_eq!(std::fs::read(&path).unwrap(), b"an active place");
    }

    #[test]
    fn output_ownership_can_be_reacquired_without_replacing_the_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        let lock_path = directory.path().join("Live.34872.rbxl.rojo-live-edit.lock");
        let owner = OutputOwnership::acquire(&path).unwrap();
        let original = File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        drop(owner);
        assert!(lock_path.exists());

        let _next_owner = OutputOwnership::acquire(&path).unwrap();
        // An already-open handle still sees the next owner's lock, proving that
        // takeover did not unlink or replace the sidecar with a different inode.
        assert!(matches!(original.try_lock(), Err(TryLockError::WouldBlock)));
        assert!(!path.exists());
    }

    #[test]
    fn output_ownership_allows_different_outputs_at_the_same_time() {
        let directory = tempfile::tempdir().unwrap();
        let _binary = OutputOwnership::acquire(&directory.path().join("Live.34872.rbxl")).unwrap();
        let _xml = OutputOwnership::acquire(&directory.path().join("Live.34872.rbxlx")).unwrap();
        let _other_port =
            OutputOwnership::acquire(&directory.path().join("Live.34873.rbxl")).unwrap();
    }

    #[test]
    fn connection_markers_preserve_specific_binds_and_bracket_ipv6() {
        for (bound, expected) in [
            ("127.0.0.1:52876", "127.0.0.1:52876"),
            ("127.0.0.2:52876", "127.0.0.2:52876"),
            ("[::1]:52876", "[::1]:52876"),
            ("0.0.0.0:52876", "127.0.0.1:52876"),
            ("[::]:52876", "[::1]:52876"),
        ] {
            assert_eq!(connection_address(bound.parse().unwrap()), expected);
        }
    }

    #[test]
    fn templates_resolve_the_port_and_require_a_place_format() {
        let place = LiveEditPlace::parse(Path::new("/tmp/ae/Live.{port}.rbxl")).unwrap();
        assert_eq!(
            place.resolve(52876),
            PathBuf::from("/tmp/ae/Live.52876.rbxl")
        );
        assert_eq!(place.kind(), OutputKind::Rbxl);
        let xml = LiveEditPlace::parse(Path::new("Live.{port}.rbxlx")).unwrap();
        assert_eq!(xml.kind(), OutputKind::Rbxlx);
        let literal_parent =
            LiveEditPlace::parse(Path::new("/tmp/{port}/Live.{port}.rbxl")).unwrap();
        assert_eq!(
            literal_parent.resolve(52876),
            PathBuf::from("/tmp/{port}/Live.52876.rbxl")
        );

        for invalid in [
            "/tmp/ae/Live.rbxl",
            "/tmp/{port}/Live.rbxl",
            "Live.{port}.rbxm",
            "Live.{port}.rbxmx",
            "{port}",
        ] {
            assert!(
                LiveEditPlace::parse(Path::new(invalid)).is_err(),
                "{invalid}"
            );
        }
        let error = LiveEditPlace::parse(Path::new("Live.rbxl")).unwrap_err();
        assert!(error.to_string().contains("must contain \"{port}\""));
    }

    #[test]
    fn written_places_carry_the_marker_that_the_tree_never_keeps() {
        let directory = tempfile::tempdir().unwrap();
        for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
            let path = directory.path().join(match kind {
                OutputKind::Rbxl => "Live.34872.rbxl",
                _ => "Live.34872.rbxlx",
            });
            let mut tree = tree();
            write(&mut tree, &path, kind, "localhost:34872", None).unwrap();

            let bytes = std::fs::read(&path).unwrap();
            let dom = match kind {
                OutputKind::Rbxl => rbx_binary::from_reader(bytes.as_slice()).unwrap(),
                _ => rbx_xml::from_reader_default(bytes.as_slice()).unwrap(),
            };
            // Attribute strings come back from files as binary strings.
            let written = workspace_attributes(&dom);
            let text = |name: &str| match written.get(name) {
                Some(Variant::String(value)) => Some(value.clone()),
                Some(Variant::BinaryString(value)) => {
                    std::str::from_utf8(value.as_ref()).ok().map(str::to_owned)
                }
                _ => None,
            };
            assert_eq!(text(MARKER_ATTRIBUTE).as_deref(), Some("localhost:34872"));
            assert_eq!(text("Existing").as_deref(), Some("kept"));

            let workspace = tree.get_instance(tree.get_root_id()).unwrap().children()[0];
            let Some(Variant::Attributes(served)) = tree
                .get_instance(workspace)
                .unwrap()
                .properties()
                .get(&ustr("Attributes"))
            else {
                panic!("The served Workspace lost its attributes.");
            };
            assert!(served.get(MARKER_ATTRIBUTE).is_none());
            assert!(served.get("Existing").is_some());
        }
    }

    #[test]
    fn directional_places_bind_session_policy_and_reset_bootstrap_without_changing_the_tree() {
        let directory = tempfile::tempdir().unwrap();
        for kind in [OutputKind::Rbxl, OutputKind::Rbxlx] {
            for direction in [None].into_iter().chain(SyncDirection::ALL.map(Some)) {
                let path = directory.path().join("Live.place");
                let mut tree = tree();
                let workspace = tree.root().children()[0];
                let original = Attributes::new()
                    .with("Existing".to_owned(), Variant::String("kept".into()))
                    .with(
                        SESSION_ATTRIBUTE.to_owned(),
                        Variant::String("obsolete".into()),
                    )
                    .with(
                        DIRECTION_ATTRIBUTE.to_owned(),
                        Variant::String("studio-to-fs".into()),
                    )
                    .with(BOOTSTRAP_ATTRIBUTE.to_owned(), Variant::Bool(true));
                tree.get_instance_mut(workspace)
                    .unwrap()
                    .properties_mut()
                    .insert(ustr("Attributes"), Variant::Attributes(original.clone()));
                let session_id = SessionId::new();
                write(
                    &mut tree,
                    &path,
                    kind,
                    "localhost:34872",
                    direction.map(|value| (session_id, value)),
                )
                .unwrap();
                let bytes = std::fs::read(&path).unwrap();
                let dom = match kind {
                    OutputKind::Rbxl => rbx_binary::from_reader(bytes.as_slice()).unwrap(),
                    _ => rbx_xml::from_reader_default(bytes.as_slice()).unwrap(),
                };
                let written = workspace_attributes(&dom);
                let text = |name| match written.get(name) {
                    Some(Variant::String(value)) => Some(value.clone()),
                    Some(Variant::BinaryString(value)) => {
                        std::str::from_utf8(value.as_ref()).ok().map(str::to_owned)
                    }
                    _ => None,
                };
                assert_eq!(text(MARKER_ATTRIBUTE).as_deref(), Some("localhost:34872"));
                assert_eq!(
                    text(SESSION_ATTRIBUTE),
                    direction.map(|_| session_id.to_string())
                );
                assert_eq!(
                    text(DIRECTION_ATTRIBUTE),
                    direction.map(|value| value.as_str().to_owned())
                );
                if let Some(value) = direction {
                    assert_eq!(serde_json::to_value(value).unwrap(), value.as_str());
                }
                assert!(written.get(BOOTSTRAP_ATTRIBUTE).is_none());
                assert_eq!(
                    tree.get_instance(workspace).unwrap().properties()[&ustr("Attributes")],
                    Variant::Attributes(original)
                );
            }
        }
    }

    #[test]
    fn writes_replace_old_places_without_leaving_staging_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        std::fs::write(&path, b"an older place").unwrap();
        write(
            &mut tree(),
            &path,
            OutputKind::Rbxl,
            "localhost:34872",
            None,
        )
        .unwrap();
        assert!(rbx_binary::from_reader(std::fs::read(&path).unwrap().as_slice()).is_ok());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn places_without_a_workspace_fail_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        let error = write(&mut tree, &path, OutputKind::Rbxl, "localhost:1", None).unwrap_err();
        assert!(error.to_string().contains("no Workspace"));
        assert!(!path.exists());
    }

    #[test]
    fn stale_places_are_removed_and_missing_ones_are_fine() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        remove_stale(&path).unwrap();
        std::fs::write(&path, b"stale").unwrap();
        remove_stale(&path).unwrap();
        assert!(!path.exists());
        std::fs::create_dir(&path).unwrap();
        assert!(remove_stale(&path).is_err());
    }
}
