//! Writes the place a live edit opens in Studio, straight from the served tree.
//!
//! The Rojo+ plugin connects with no UI to the address stored in the Workspace
//! attribute below. Writing the place from the tree `rojo serve` already loaded
//! saves a second project load and a separate marking pass.

use std::{
    io,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use rbx_dom_weak::{
    types::{Attributes, Variant},
    ustr,
};

use crate::snapshot::RojoTree;

use super::{build::OutputKind, display, export::write_output};

/// Read by the plugin's `App:readLiveEditMarker`, formatted as `host:port`.
pub(super) const MARKER_ATTRIBUTE: &str = "__RojoPlus_LiveEditUrl";
const PORT_PLACEHOLDER: &str = "{port}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveEditPlace {
    template: String,
    kind: OutputKind,
}

impl LiveEditPlace {
    /// Validates a path template before the project loads, so a bad flag fails
    /// fast. The port is part of the name so several servers never share a file.
    pub(super) fn parse(template: &Path) -> anyhow::Result<Self> {
        let shown = display::display_path(
            &template.to_string_lossy(),
            display::home_dir().as_deref(),
            false,
        );
        let Some(text) = template.to_str() else {
            bail!("The live edit place path \"{shown}\" is not valid UTF-8.");
        };
        if !text.contains(PORT_PLACEHOLDER) {
            bail!(
                "The live edit place path \"{shown}\" must contain \"{PORT_PLACEHOLDER}\", which \
                 is replaced by the server's port."
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
        PathBuf::from(self.template.replace(PORT_PLACEHOLDER, &port.to_string()))
    }

    pub(super) fn kind(&self) -> OutputKind {
        self.kind
    }
}

/// Deletes a place left over from an earlier server on the same port. A consumer
/// waits for the file to appear, so an old copy would open with a stale tree.
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
    fn templates_resolve_the_port_and_require_a_place_format() {
        let place = LiveEditPlace::parse(Path::new("/tmp/ae/Live.{port}.rbxl")).unwrap();
        assert_eq!(
            place.resolve(52876),
            PathBuf::from("/tmp/ae/Live.52876.rbxl")
        );
        assert_eq!(place.kind(), OutputKind::Rbxl);
        let xml = LiveEditPlace::parse(Path::new("Live.{port}.rbxlx")).unwrap();
        assert_eq!(xml.kind(), OutputKind::Rbxlx);

        for invalid in [
            "/tmp/ae/Live.rbxl",
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
            write(&mut tree, &path, kind, "localhost:34872").unwrap();

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
    fn writes_replace_old_places_without_leaving_staging_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        std::fs::write(&path, b"an older place").unwrap();
        write(&mut tree(), &path, OutputKind::Rbxl, "localhost:34872").unwrap();
        assert!(rbx_binary::from_reader(std::fs::read(&path).unwrap().as_slice()).is_ok());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn places_without_a_workspace_fail_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Live.34872.rbxl");
        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        let error = write(&mut tree, &path, OutputKind::Rbxl, "localhost:1").unwrap_err();
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
