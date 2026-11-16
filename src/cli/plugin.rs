use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};

use anyhow::bail;
use clap::Parser;
use memofs::{InMemoryFs, Vfs, VfsSnapshot};
use roblox_install::RobloxStudio;

use crate::serve_session::ServeSession;

static PLUGIN_BINCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugin.bincode"));
/// Differs from upstream Rojo's `RojoManagedPlugin.rbxm`, so both plugins can be
/// installed side by side without overwriting each other.
static PLUGIN_FILE_NAME: &str = "Rojo+.rbxm";

/// Install Rojo+'s plugin.
#[derive(Debug, Parser)]
pub struct PluginCommand {
    #[clap(subcommand)]
    subcommand: PluginSubcommand,
}

/// Manages Rojo+'s Roblox Studio plugin.
#[derive(Debug, Parser)]
pub enum PluginSubcommand {
    /// Install the plugin in Roblox Studio's plugins folder. If the plugin is
    /// already installed, installing it again will overwrite the current plugin
    /// file.
    Install {
        /// Install into this plugins folder instead of Roblox Studio's default.
        /// The folder must already exist.
        #[clap(long)]
        path: Option<PathBuf>,
    },

    /// Removes the plugin if it is installed.
    Uninstall {
        /// Remove the plugin from this plugins folder instead of Roblox
        /// Studio's default. The folder must already exist.
        #[clap(long)]
        path: Option<PathBuf>,
    },
}

impl PluginCommand {
    pub fn run(self) -> anyhow::Result<()> {
        self.subcommand.run()
    }
}

impl PluginSubcommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            PluginSubcommand::Install { path } => install_plugin(path),
            PluginSubcommand::Uninstall { path } => uninstall_plugin(path),
        }
    }
}

/// Where the plugins folder lives and whether Rojo+ may create it.
#[derive(Debug, PartialEq, Eq)]
struct PluginsFolder {
    path: PathBuf,
    /// Studio's default folder may not exist until a plugin is installed, so
    /// Rojo+ creates it. A folder the user names explicitly is never created,
    /// since a typo would otherwise install the plugin somewhere Studio never
    /// looks.
    create_if_missing: bool,
}

fn resolve_plugins_folder(
    explicit: Option<PathBuf>,
    locate_default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PluginsFolder> {
    Ok(match explicit {
        Some(path) => PluginsFolder {
            path,
            create_if_missing: false,
        },
        None => PluginsFolder {
            path: locate_default()?,
            create_if_missing: true,
        },
    })
}

fn locate_studio_plugins_folder() -> anyhow::Result<PathBuf> {
    Ok(RobloxStudio::locate()?.plugins_path().to_path_buf())
}

/// Rejects a missing folder the user named explicitly, so a typo fails for
/// both install and uninstall instead of silently doing nothing useful.
fn check_explicit_folder(folder: &PluginsFolder) -> anyhow::Result<()> {
    if !folder.create_if_missing && !folder.path.is_dir() {
        bail!(
            "The plugins folder \"{}\" does not exist.",
            folder.path.display()
        );
    }
    Ok(())
}

fn ensure_plugins_folder(folder: &PluginsFolder) -> anyhow::Result<()> {
    check_explicit_folder(folder)?;
    if folder.path.is_dir() {
        return Ok(());
    }
    log::debug!("Creating Roblox Studio plugins folder");
    fs::create_dir(&folder.path)?;
    Ok(())
}

fn plugin_file_path(folder: &Path) -> PathBuf {
    folder.join(PLUGIN_FILE_NAME)
}

fn initialize_plugin() -> anyhow::Result<ServeSession> {
    let plugin_snapshot: VfsSnapshot = bincode::deserialize(PLUGIN_BINCODE)
        .expect("Rojo+'s plugin was not properly packed into Rojo+'s binary");

    let mut in_memory_fs = InMemoryFs::new();
    in_memory_fs.load_snapshot("/plugin", plugin_snapshot)?;

    let vfs = Vfs::new(in_memory_fs);
    Ok(ServeSession::new(vfs, "/plugin")?)
}

fn install_plugin(path: Option<PathBuf>) -> anyhow::Result<()> {
    let folder = resolve_plugins_folder(path, locate_studio_plugins_folder)?;
    ensure_plugins_folder(&folder)?;

    let plugin_path = plugin_file_path(&folder.path);
    log::debug!("Writing plugin to {}", plugin_path.display());

    let mut file = BufWriter::new(File::create(plugin_path)?);

    let session = initialize_plugin()?;
    let tree = session.tree();
    let root_id = tree.get_root_id();

    rbx_binary::to_writer(&mut file, tree.inner(), &[root_id])?;

    Ok(())
}

fn uninstall_plugin(path: Option<PathBuf>) -> anyhow::Result<()> {
    let folder = resolve_plugins_folder(path, locate_studio_plugins_folder)?;
    check_explicit_folder(&folder)?;

    let plugin_path = plugin_file_path(&folder.path);

    if plugin_path.exists() {
        log::debug!("Removing existing plugin from {}", plugin_path.display());
        fs::remove_file(plugin_path)?;
    } else {
        log::debug!("Plugin not installed at {}", plugin_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn plugin_initialize() {
        let _ = initialize_plugin().unwrap();
    }

    /// Studio only runs a local plugin's scripts when their RunContext is Plugin
    /// or Legacy, so an entry script built as a Server script loads but never
    /// runs.
    #[test]
    fn the_plugin_entry_script_runs_in_the_plugin_context() {
        use rbx_dom_weak::{
            types::{Enum, Variant},
            ustr,
        };

        let session = initialize_plugin().unwrap();
        let tree = session.tree();
        let dom = tree.inner();
        let root = dom.get_by_ref(tree.get_root_id()).unwrap();
        let entry = root
            .children()
            .iter()
            .map(|id| dom.get_by_ref(*id).unwrap())
            .find(|instance| instance.name == "Plugin")
            .expect("The plugin should have a \"Plugin\" entry script");

        let plugin_context = *rbx_reflection_database::get()
            .unwrap()
            .enums
            .get("RunContext")
            .unwrap()
            .items
            .get("Plugin")
            .unwrap();

        assert_eq!(entry.class, "Script");
        assert_eq!(
            entry.properties.get(&ustr("RunContext")),
            Some(&Variant::Enum(Enum::from_u32(plugin_context)))
        );
    }

    #[test]
    fn the_plugin_never_overwrites_upstream_rojo() {
        assert_eq!(PLUGIN_FILE_NAME, "Rojo+.rbxm");
        for upstream in ["Rojo.rbxm", "RojoManagedPlugin.rbxm"] {
            assert_ne!(PLUGIN_FILE_NAME, upstream);
        }
    }

    #[test]
    fn an_explicit_path_skips_studio_and_is_never_created() {
        let folder = resolve_plugins_folder(Some(PathBuf::from("custom")), || {
            panic!("Studio should not be located when a path is given")
        })
        .unwrap();
        assert_eq!(
            folder,
            PluginsFolder {
                path: PathBuf::from("custom"),
                create_if_missing: false,
            }
        );
    }

    #[test]
    fn no_path_uses_the_studio_folder_and_creates_it() {
        let folder = resolve_plugins_folder(None, || Ok(PathBuf::from("studio"))).unwrap();
        assert_eq!(
            folder,
            PluginsFolder {
                path: PathBuf::from("studio"),
                create_if_missing: true,
            }
        );
    }

    #[test]
    fn no_path_propagates_studio_lookup_errors() {
        let result = resolve_plugins_folder(None, || anyhow::bail!("no studio"));
        assert!(result.is_err());
    }

    #[test]
    fn a_missing_explicit_folder_is_rejected() {
        let missing = tempfile::tempdir().unwrap().path().join("missing");
        let folder = resolve_plugins_folder(Some(missing.clone()), || unreachable!()).unwrap();
        let error = ensure_plugins_folder(&folder).unwrap_err().to_string();
        assert!(error.contains("does not exist"));
        assert!(!missing.exists());
    }

    #[test]
    fn a_missing_default_folder_is_created() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("Plugins");
        let folder = resolve_plugins_folder(None, || Ok(plugins.clone())).unwrap();
        ensure_plugins_folder(&folder).unwrap();
        assert!(plugins.is_dir());
    }

    #[test]
    fn only_a_missing_explicit_folder_fails_the_check() {
        let root = tempfile::tempdir().unwrap();
        for create_if_missing in [false, true] {
            for exists in [false, true] {
                let path = root.path().join(format!("{create_if_missing}-{exists}"));
                if exists {
                    fs::create_dir(&path).unwrap();
                }
                let folder = PluginsFolder {
                    path: path.clone(),
                    create_if_missing,
                };
                let result = check_explicit_folder(&folder);
                assert_eq!(result.is_err(), !create_if_missing && !exists);
                assert_eq!(path.exists(), exists, "The check must never create folders");
            }
        }
    }

    #[test]
    fn the_plugin_file_goes_inside_the_folder() {
        assert_eq!(
            plugin_file_path(Path::new("custom")),
            Path::new("custom").join("Rojo+.rbxm")
        );
    }
}
