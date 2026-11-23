//! Shared styling for the messages Rojo+ prints to the terminal.
//!
//! The palette matches the commandline-luau theme used by the tools that run Rojo+,
//! so relayed output reads as one program. Roblox files are bright cyan and bold,
//! other paths, addresses, and names are muted, and warnings are yellow.

use std::{
    io::{self, Write},
    net::IpAddr,
};

use termcolor::{Buffer, Color, ColorSpec, WriteColor};

fn muted() -> ColorSpec {
    let mut spec = ColorSpec::new();
    spec.set_fg(Some(Color::Black)).set_intense(true);
    spec
}

fn roblox_path(bold: bool) -> ColorSpec {
    let mut spec = ColorSpec::new();
    spec.set_fg(Some(Color::Cyan))
        .set_intense(true)
        .set_bold(bold);
    spec
}

pub(super) fn warning() -> ColorSpec {
    let mut spec = ColorSpec::new();
    spec.set_fg(Some(Color::Yellow));
    spec
}

/// Writes `text` in the muted color, then returns to the default style.
pub(super) fn write_muted(buffer: &mut Buffer, text: &str) -> io::Result<()> {
    buffer.set_color(&muted())?;
    write!(buffer, "{text}")?;
    buffer.reset()
}

/// Writes a Roblox file path in bright cyan bold. Spaces become a center dot that
/// is never bold, so names with spaces stay readable as one path.
pub(super) fn write_roblox_path(buffer: &mut Buffer, text: &str) -> io::Result<()> {
    for (index, part) in text.split(' ').enumerate() {
        if index > 0 {
            buffer.set_color(&roblox_path(false))?;
            write!(buffer, "·")?;
        }
        buffer.set_color(&roblox_path(true))?;
        write!(buffer, "{part}")?;
    }
    buffer.reset()
}

/// The host a person or the Studio plugin should use to reach a server bound to
/// `ip`. IPv6 hosts are bracketed so `host:port` stays unambiguous.
pub(super) fn display_host(ip: IpAddr) -> String {
    if ip.is_loopback() {
        "localhost".to_owned()
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn display_host_names_loopback_and_brackets_ipv6() {
        assert_eq!(display_host(Ipv4Addr::LOCALHOST.into()), "localhost");
        assert_eq!(display_host(Ipv6Addr::LOCALHOST.into()), "localhost");
        assert_eq!(display_host(Ipv4Addr::new(10, 0, 0, 2).into()), "10.0.0.2");
        assert_eq!(
            display_host("fe80::1".parse::<IpAddr>().unwrap()),
            "[fe80::1]"
        );
    }

    #[test]
    fn roblox_paths_dot_spaces_without_bold() {
        let mut buffer = Buffer::ansi();
        write_roblox_path(&mut buffer, "My Place.rbxl").unwrap();
        let text = String::from_utf8(buffer.into_inner()).unwrap();
        let dot = text.find('·').unwrap();
        let before_dot = &text[..dot];
        let last_style = &before_dot[before_dot.rfind("\x1b[0m").unwrap()..];
        assert!(!last_style.contains("\x1b[1m"), "{text:?}");
        assert!(text.contains("My") && text.contains("Place.rbxl"));
        assert!(text.ends_with("\x1b[0m"));
    }

    #[test]
    fn styled_text_is_plain_without_color() {
        let mut buffer = Buffer::no_color();
        write_muted(&mut buffer, "localhost:34872").unwrap();
        write_roblox_path(&mut buffer, "A B.rbxl").unwrap();
        assert_eq!(
            String::from_utf8(buffer.into_inner()).unwrap(),
            "localhost:34872A·B.rbxl"
        );
    }
}
