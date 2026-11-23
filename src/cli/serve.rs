use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::PathBuf,
    sync::Arc,
};

use clap::Parser;
use memofs::Vfs;
use termcolor::{Buffer, BufferWriter, ColorChoice, WriteColor};

use crate::{serve_session::ServeSession, web::LiveServer};

use super::{display, resolve_path, GlobalOptions};

const DEFAULT_BIND_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const DEFAULT_PORT: u16 = 34872;
const MAX_PORT_ATTEMPTS: u16 = 100;

/// Expose a Rojo+ project to the Rojo+ Studio plugin.
#[derive(Debug, Parser)]
pub struct ServeCommand {
    /// Path to the project to serve. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// The IP address to listen on. Defaults to `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// The port to listen on. Defaults to the project's preference, or `34872` if
    /// it has none.
    #[clap(long)]
    pub port: Option<u16>,

    /// Extra `Host`/`Origin` values the server will accept, beyond localhost and
    /// the bind address (for example a hostname like `mypc.lan`). Repeat the
    /// option or comma-separate to allow several. When given, this overrides the
    /// project's `serveAllowedHosts`. Listing any host also turns on Host/Origin
    /// validation for binds where it is otherwise off (such as `0.0.0.0`).
    #[clap(long, value_delimiter = ',')]
    pub allowed_hosts: Vec<String>,

    /// Stop the server when standard input closes or encounters a read error.
    #[clap(long)]
    pub shutdown_on_stdin_eof: bool,
}

impl ServeCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?;

        let vfs = Vfs::new_default()?;

        let session = Arc::new(ServeSession::new(vfs, project_path)?);

        let (ip, port) = resolve_bind_address(&session, self.address, self.port)?;

        // The CLI flag, when given, replaces the project's list rather than
        // merging with it, matching how --address and --port override theirs.
        let allowed_hosts = if self.allowed_hosts.is_empty() {
            session.serve_allowed_hosts().to_vec()
        } else {
            self.allowed_hosts
        };

        let server = LiveServer::new(session);

        let on_listening = |address: std::net::SocketAddr| {
            let _ = show_start_message(address.ip(), address.port(), global.color.into());
        };

        if self.shutdown_on_stdin_eof {
            let (sender, receiver) = futures::channel::oneshot::channel();
            // Keep blocking stdin reads outside Tokio so runtime teardown never waits for input.
            std::thread::Builder::new()
                .name("rojo-stdin-shutdown".into())
                .spawn(move || {
                    wait_for_stdin_eof(io::stdin().lock());
                    let _ = sender.send(());
                })?;
            server.start_with_shutdown((ip, port).into(), allowed_hosts, on_listening, async {
                let _ = receiver.await;
            })?;
        } else {
            server.start((ip, port).into(), allowed_hosts, on_listening)?;
        }

        Ok(())
    }
}

/// Picks the address and port to listen on. Flags win over the project's
/// preferences. An explicit port is used as-is, while the default one moves up
/// to the next free port, so a second server can run next to `rojo serve`.
pub(super) fn resolve_bind_address(
    session: &ServeSession,
    address: Option<IpAddr>,
    port: Option<u16>,
) -> anyhow::Result<(IpAddr, u16)> {
    let ip = address
        .or_else(|| session.serve_address())
        .unwrap_or(DEFAULT_BIND_ADDRESS.into());

    let port = match port {
        Some(port) => port,
        None => find_available_port(ip, session.project_port().unwrap_or(DEFAULT_PORT))?,
    };

    Ok((ip, port))
}

fn wait_for_stdin_eof(mut input: impl Read) {
    let mut buffer = [0; 4096];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn find_available_port(ip: IpAddr, starting_port: u16) -> anyhow::Result<u16> {
    for offset in 0..MAX_PORT_ATTEMPTS {
        let port = starting_port
            .checked_add(offset)
            .ok_or_else(|| anyhow::anyhow!("Port number overflow"))?;

        if TcpListener::bind((ip, port)).is_ok() {
            return Ok(port);
        }
    }

    anyhow::bail!(
        "Could not find an available port in range {}-{}",
        starting_port,
        starting_port.saturating_add(MAX_PORT_ATTEMPTS - 1)
    )
}

fn show_start_message(bind_address: IpAddr, port: u16, color: ColorChoice) -> io::Result<()> {
    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();
    write_start_message(&mut buffer, bind_address, port)?;
    writer.print(&buffer)
}

fn write_start_message(buffer: &mut Buffer, bind_address: IpAddr, port: u16) -> io::Result<()> {
    let host = display::display_host(bind_address);

    write!(buffer, "Rojo+ server listening on ")?;
    display::write_muted(buffer, &format!("{host}:{port}"))?;
    writeln!(buffer, ".")?;

    write!(buffer, "Visit ")?;
    display::write_muted(buffer, &format!("http://{host}:{port}/"))?;
    writeln!(buffer, " in your browser for more information.")?;

    if !bind_address.is_loopback() {
        buffer.set_color(&display::warning())?;
        write!(
            buffer,
            "This server is bound to {host}, which is reachable from the network. The serve \
             API is unauthenticated, so anyone who can reach {host}:{port} can read and modify \
             your project's source. Prefer binding to localhost and tunneling (for example \
             over SSH, Tailscale, or WireGuard) when you need remote access."
        )?;
        buffer.reset()?;
        writeln!(buffer)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_shutdown_is_opt_in() {
        assert!(
            !ServeCommand::try_parse_from(["serve"])
                .unwrap()
                .shutdown_on_stdin_eof
        );
        assert!(
            ServeCommand::try_parse_from(["serve", "--shutdown-on-stdin-eof"])
                .unwrap()
                .shutdown_on_stdin_eof
        );
    }

    fn start_message(bind_address: IpAddr, port: u16, buffer: Buffer) -> String {
        let mut buffer = buffer;
        write_start_message(&mut buffer, bind_address, port).unwrap();
        String::from_utf8(buffer.into_inner()).unwrap()
    }

    #[test]
    fn start_message_is_two_sentences_on_loopback() {
        assert_eq!(
            start_message(DEFAULT_BIND_ADDRESS.into(), 52876, Buffer::no_color()),
            "Rojo+ server listening on localhost:52876.\n\
             Visit http://localhost:52876/ in your browser for more information.\n"
        );
    }

    #[test]
    fn start_message_keeps_the_url_contiguous_inside_its_style() {
        let text = start_message(DEFAULT_BIND_ADDRESS.into(), 52876, Buffer::ansi());
        // Tools that relay this output find the address with a pattern like
        // `http://host:port/`, so styling must surround the URL, never split it.
        assert!(text.contains("http://localhost:52876/\x1b[0m in your browser"));
        assert!(text.contains("localhost:52876\x1b[0m.\n"));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn start_message_warns_in_sentences_when_reachable_from_the_network() {
        let text = start_message(Ipv4Addr::UNSPECIFIED.into(), 34872, Buffer::no_color());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Rojo+ server listening on 0.0.0.0:34872.");
        assert!(lines[2].starts_with("This server is bound to 0.0.0.0, which is reachable"));
        assert!(lines[2].ends_with("when you need remote access."));
        assert!(!text.contains("WARNING"));
    }

    #[test]
    fn stdin_reader_retries_interruptions_and_stops_on_eof_or_error() {
        struct Input {
            reads: usize,
            fail: bool,
        }

        impl Read for Input {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                match self.reads {
                    1 => Err(io::ErrorKind::Interrupted.into()),
                    2 | 3 => {
                        buffer[0] = b'x';
                        Ok(1)
                    }
                    4 if self.fail => Err(io::ErrorKind::BrokenPipe.into()),
                    4 => Ok(0),
                    _ => panic!("Read after stdin ended"),
                }
            }
        }

        for fail in [false, true] {
            let mut input = Input { reads: 0, fail };
            wait_for_stdin_eof(&mut input);
            assert_eq!(input.reads, 4);
        }
    }
}
