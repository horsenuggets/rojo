use std::{
    io::{self, BufReader, Write as _},
    mem::forget,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::Context;
use clap::Parser;
use fs_err::File;
use memofs::Vfs;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

use crate::{
    path_serializer::display_absolute,
    serve_session::ServeSession,
    syncback::{
        from_studio::{inherit_service_properties, studio_services},
        syncback_loop, FsSnapshot,
    },
    web::studio_syncback,
};

use super::{resolve_path, serve::resolve_bind_address, GlobalOptions};

const UNKNOWN_INPUT_KIND_ERR: &str = "Could not detect what kind of file was inputted. \
                                       Expected input file to end in .rbxl, .rbxlx, .rbxm, or .rbxmx.";

/// Performs 'syncback' for the provided project, using the `input` file
/// given.
///
/// Syncback exists to convert Roblox files into a Rojo+ project automatically.
/// It uses the project.json file provided to traverse the Roblox file passed as
/// to serialize Instances to the file system in a format that Rojo+ understands.
///
/// To ease programmatic use, this command pipes all normal output to stderr.
#[derive(Debug, Parser)]
pub struct SyncbackCommand {
    /// Path to the project to sync back to.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Path to the Roblox file to pull Instances from.
    #[clap(long, short, required_unless_present = "from-studio")]
    pub input: Option<PathBuf>,

    /// If provided, Instances are pulled from the place open in Studio instead
    /// of a file. Rojo+ waits for the Studio plugin to upload the place.
    #[clap(long, conflicts_with = "input")]
    pub from_studio: bool,

    /// The IP address to wait for Studio on, with `--from-studio`. Defaults to
    /// `127.0.0.1`.
    #[clap(long, requires = "from-studio", conflicts_with = "input")]
    pub address: Option<IpAddr>,

    /// The port to wait for Studio on, with `--from-studio`. Defaults to the
    /// project's preference, or the first free port from `34872`.
    #[clap(long, requires = "from-studio", conflicts_with = "input")]
    pub port: Option<u16>,

    /// If provided, a list all of the files and directories that will be
    /// added or removed is emitted into stdout.
    #[clap(long, short)]
    pub list: bool,

    /// If provided, syncback will not actually write anything to the file
    /// system. The command will otherwise run normally.
    #[clap(long)]
    pub dry_run: bool,

    /// If provided, the prompt for writing to the file system is skipped.
    #[clap(long, short = 'y')]
    pub non_interactive: bool,
}

impl SyncbackCommand {
    pub fn run(&self, global: GlobalOptions) -> anyhow::Result<()> {
        let path_old = resolve_path(&self.project)?;

        let vfs = Vfs::new_default()?;
        vfs.set_watch_enabled(false);

        let project_start_timer = Instant::now();
        let session_old = ServeSession::new(vfs, path_old.clone())?;
        log::debug!(
            "Finished opening project in {:0.02}s",
            project_start_timer.elapsed().as_secs_f32()
        );

        let dom_start_timer = Instant::now();
        let dom_new = match &self.input {
            Some(input) => {
                let path_new = resolve_path(input)?;
                let input_kind = FileKind::from_path(&path_new).context(UNKNOWN_INPUT_KIND_ERR)?;
                read_dom(&path_new, input_kind)?
            }
            None => self.receive_from_studio(&session_old, global.color.into())?,
        };
        log::debug!(
            "Finished reading the place in {:0.02}s",
            dom_start_timer.elapsed().as_secs_f32()
        );

        let mut dom_old = session_old.tree();

        log::debug!("Old root: {}", dom_old.inner().root().class);
        log::debug!("New root: {}", dom_new.root().class);

        if log::log_enabled!(log::Level::Trace) {
            log::trace!("Children of old root:");
            for child in dom_old.inner().root().children() {
                let inst = dom_old.get_instance(*child).unwrap();
                log::trace!("{} (class: {})", inst.name(), inst.class_name());
            }
            log::trace!("Children of new root:");
            for child in dom_new.root().children() {
                let inst = dom_new.get_by_ref(*child).unwrap();
                log::trace!("{} (class: {})", inst.name, inst.class);
            }
        }

        let syncback_timer = Instant::now();
        eprintln!("Beginning syncback...");
        let snapshot = syncback_loop(
            session_old.vfs(),
            &mut dom_old,
            dom_new,
            session_old.root_project(),
        )?;
        log::debug!(
            "Syncback finished in {:.02}s!",
            syncback_timer.elapsed().as_secs_f32()
        );

        let base_path = session_old.root_project().folder_location();
        if self.list {
            list_files(&snapshot, global.color.into(), base_path)?;
        }

        if !self.dry_run {
            if !self.non_interactive {
                eprintln!(
                    "Would write {} files/folders and remove {} files/folders.",
                    snapshot.added_paths().len(),
                    snapshot.removed_paths().len()
                );
                eprint!("Is this okay? (Y/N): ");
                io::stderr().flush()?;
                let mut line = String::with_capacity(1);
                io::stdin().read_line(&mut line)?;
                line = line.trim().to_lowercase();
                if line != "y" {
                    eprintln!("Aborting due to user input!");
                    return Ok(());
                }
            }
            eprintln!("Writing to the file system...");
            snapshot.write_to_vfs(base_path, session_old.vfs())?;
            eprintln!("Finished syncback.")
        } else {
            eprintln!(
                "Would write {} files/folders and remove {} files/folders.",
                snapshot.added_paths().len(),
                snapshot.removed_paths().len()
            );
            eprintln!("Aborting before writing to file system due to `--dry-run`");
        }

        // It is potentially prohibitively expensive to drop a ServeSession,
        // and the program is about to exit anyway so we're just going to forget
        // about it.
        drop(dom_old);
        forget(session_old);

        Ok(())
    }

    /// Waits for the Studio plugin to upload the open place, then rebuilds it.
    fn receive_from_studio(
        &self,
        session: &ServeSession,
        color: ColorChoice,
    ) -> anyhow::Result<WeakDom> {
        let services = studio_services(session.tree().inner())?;
        let (ip, port) = resolve_bind_address(session, self.address, self.port)?;

        let place = studio_syncback::receive_place(
            (ip, port).into(),
            session.serve_allowed_hosts(),
            session.project_name().to_owned(),
            services,
            |address| {
                let _ = show_waiting_message(address.ip(), address.port(), color);
            },
        )?;

        let mut dom = place.dom;
        let instance_count = dom.descendants().count() - 1;
        eprintln!(
            "Received {} from Studio.",
            plural(instance_count, "instance", "instances")
        );
        if !place.dropped.is_empty() {
            eprintln!(
                "Studio couldn't serialize {}, so {} skipped.",
                plural(place.dropped.len(), "instance", "instances"),
                if place.dropped.len() == 1 {
                    "it was"
                } else {
                    "they were"
                }
            );
            for entry in &place.dropped {
                eprintln!("  {}.{} ({})", entry.service, entry.name, entry.class_name);
            }
        }

        inherit_service_properties(&mut dom, session.tree().inner());
        Ok(dom)
    }
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn show_waiting_message(ip: IpAddr, port: u16, color: ColorChoice) -> io::Result<()> {
    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green)).set_bold(true);

    let writer = BufferWriter::stderr(color);
    let mut buffer = writer.buffer();

    let address = if ip.is_loopback() {
        "localhost".to_owned()
    } else {
        ip.to_string()
    };

    write!(&mut buffer, "Waiting for Studio on ")?;
    buffer.set_color(&green)?;
    write!(&mut buffer, "{address}:{port}")?;
    buffer.reset()?;
    writeln!(&mut buffer, ".")?;
    writeln!(
        &mut buffer,
        "Run \"shared.RojoPlus.syncbackFromStudio(\"{address}\", {port})\" in Studio to send the \
         open place."
    )?;

    writer.print(&buffer)
}

fn read_dom(path: &Path, file_kind: FileKind) -> anyhow::Result<WeakDom> {
    let content = BufReader::new(File::open(path)?);
    match file_kind {
        FileKind::Rbxl => rbx_binary::from_reader(content).with_context(|| {
            format!(
                "Could not deserialize binary place file at {}",
                path.display()
            )
        }),
        FileKind::Rbxlx => rbx_xml::from_reader(content, xml_decode_config())
            .with_context(|| format!("Could not deserialize XML place file at {}", path.display())),
        FileKind::Rbxm => {
            let temp_tree = rbx_binary::from_reader(content).with_context(|| {
                format!(
                    "Could not deserialize binary place file at {}",
                    path.display()
                )
            })?;

            process_model_dom(temp_tree)
        }
        FileKind::Rbxmx => {
            let temp_tree =
                rbx_xml::from_reader(content, xml_decode_config()).with_context(|| {
                    format!("Could not deserialize XML model file at {}", path.display())
                })?;
            process_model_dom(temp_tree)
        }
    }
}

fn process_model_dom(dom: WeakDom) -> anyhow::Result<WeakDom> {
    let temp_children = dom.root().children();
    if temp_children.len() == 1 {
        let real_root = dom.get_by_ref(temp_children[0]).unwrap();
        let mut new_tree = WeakDom::new(InstanceBuilder::new(real_root.class));
        for (name, property) in &real_root.properties {
            new_tree
                .root_mut()
                .properties
                .insert(*name, property.to_owned());
        }

        let children = dom.clone_multiple_into_external(real_root.children(), &mut new_tree);
        for child in children {
            new_tree.transfer_within(child, new_tree.root_ref());
        }
        Ok(new_tree)
    } else {
        anyhow::bail!(
            "Rojo+ does not currently support models with more \
        than one Instance at the Root!"
        );
    }
}

fn xml_decode_config() -> rbx_xml::DecodeOptions<'static> {
    rbx_xml::DecodeOptions::new().property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown)
}

/// The different kinds of input that Rojo+ can syncback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    /// An XML model file.
    Rbxmx,

    /// An XML place file.
    Rbxlx,

    /// A binary model file.
    Rbxm,

    /// A binary place file.
    Rbxl,
}

impl FileKind {
    fn from_path(output: &Path) -> Option<FileKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxlx" => Some(FileKind::Rbxlx),
            "rbxmx" => Some(FileKind::Rbxmx),
            "rbxl" => Some(FileKind::Rbxl),
            "rbxm" => Some(FileKind::Rbxm),
            _ => None,
        }
    }
}

fn list_files(snapshot: &FsSnapshot, color: ColorChoice, base_path: &Path) -> io::Result<()> {
    let no_color = ColorSpec::new();
    let mut add_color = ColorSpec::new();
    add_color.set_fg(Some(Color::Green));
    let mut remove_color = ColorSpec::new();
    remove_color.set_fg(Some(Color::Red));

    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();

    let added = snapshot.added_paths();
    if !added.is_empty() {
        buffer.set_color(&add_color)?;
        for path in added {
            writeln!(
                &mut buffer,
                "Writing {}",
                display_absolute(path.strip_prefix(base_path).unwrap_or(path))
            )?;
        }
    }
    let removed = snapshot.removed_paths();
    if !removed.is_empty() {
        buffer.set_color(&remove_color)?;
        for path in removed {
            writeln!(
                &mut buffer,
                "Removing {}",
                display_absolute(path.strip_prefix(base_path).unwrap_or(path))
            )?;
        }
    }
    buffer.set_color(&no_color)?;

    writer.print(&buffer)
}
