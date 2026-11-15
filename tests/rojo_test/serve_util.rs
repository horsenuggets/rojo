use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use hyper_tungstenite::tungstenite::{connect, Message};
use rbx_dom_weak::types::Ref;

use serde::{Deserialize, Serialize};
use tempfile::{tempdir, TempDir};

use librojo::{
    web_api::{
        ReadResponse, SerializeRequest, SerializeResponse, ServerInfoResponse, SocketPacket,
        SocketPacketType, WriteRequest, WriteResponse,
    },
    SessionId,
};
use rojo_insta_ext::RedactionMap;

use crate::rojo_test::io_util::{
    copy_recursive, get_working_dir_path, KillOnDrop, ROJO_PATH, SERVE_TESTS_PATH,
};

/// Convenience method to run a `rojo serve` test.
///
/// Test projects should be defined in the `serve-tests` folder; their filename
/// should be given as the first parameter.
///
/// The passed in callback is where the actual test body should go. Setup and
/// cleanup happens automatically.
pub fn run_serve_test(test_name: &str, callback: impl FnOnce(TestServeSession, RedactionMap)) {
    let _ = env_logger::try_init();

    let mut redactions = RedactionMap::default();

    let mut session = TestServeSession::new(test_name);
    let info = session.wait_to_come_online();

    redactions.intern(info.session_id);
    redactions.intern(info.root_instance_id);

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(SERVE_TESTS_PATH)
        .parent()
        .unwrap()
        .join("serve-test-snapshots");

    settings.set_snapshot_path(snapshot_path);
    settings.set_sort_maps(true);
    settings.add_redaction(".serverVersion", "[server-version]");
    settings.bind(move || callback(session, redactions));
}

/// Represents a running Rojo serve session running in a temporary directory.
pub struct TestServeSession {
    // Drop order is important here: we want the process to be killed before the
    // directory it's operating on is destroyed.
    rojo_process: KillOnDrop,
    _dir: TempDir,

    port: usize,
    project_path: PathBuf,
}

impl TestServeSession {
    pub fn new(name: &str) -> Self {
        let working_dir = get_working_dir_path();

        let source_path = Path::new(SERVE_TESTS_PATH).join(name);
        let dir = tempdir().expect("Couldn't create temporary directory");
        let project_path = dir
            .path()
            .canonicalize()
            .expect("Couldn't canonicalize temporary directory path")
            .join(name);

        let source_is_file = fs::metadata(&source_path).unwrap().is_file();

        if source_is_file {
            fs::copy(&source_path, &project_path).expect("couldn't copy project file");
        } else {
            fs::create_dir(&project_path).expect("Couldn't create temporary project subdirectory");

            copy_recursive(&source_path, &project_path)
                .expect("Couldn't copy project to temporary directory");
        };

        // This is an ugly workaround for FSEvents sometimes reporting events
        // for the above copy operations, similar to this Stack Overflow question:
        // https://stackoverflow.com/questions/47679298/howto-avoid-receiving-old-events-in-fseventstream-callback-fsevents-framework-o
        // We'll hope that 100ms is enough for FSEvents to get whatever it is
        // out of its system.
        // TODO: find a better way to avoid processing these spurious events.
        #[cfg(target_os = "macos")]
        std::thread::sleep(Duration::from_millis(100));

        let mut rojo_process = KillOnDrop(
            Command::new(ROJO_PATH)
                .args([
                    "--color",
                    "never",
                    "serve",
                    project_path.to_str().unwrap(),
                    "--port",
                    "0",
                ])
                .current_dir(working_dir)
                .stdout(Stdio::piped())
                .spawn()
                .expect("Couldn't start Rojo"),
        );
        let port = read_server_port(rojo_process.0.stdout.take().unwrap(), "Port:");

        TestServeSession {
            rojo_process,
            _dir: dir,
            port,
            project_path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.project_path
    }

    pub fn port(&self) -> usize {
        self.port
    }

    /// Waits for the `rojo serve` server to come online with expontential
    /// backoff.
    pub fn wait_to_come_online(&mut self) -> ServerInfoResponse {
        const BASE_DURATION_MS: f32 = 30.0;
        const EXP_BACKOFF_FACTOR: f32 = 1.3;
        const MAX_RETRY_MS: f32 = 500.0;
        // A fixed retry count gave the server well under a second, which a busy
        // machine can exceed. Exiting early is still detected on every attempt.
        const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

        let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
        let mut i = 0;
        while std::time::Instant::now() < deadline {
            i += 1;
            match self.rojo_process.0.try_wait() {
                Ok(Some(status)) => panic!("Rojo process exited with status {}", status),
                Ok(None) => { /* The process is still running, as expected */ }
                Err(err) => panic!("Failed to wait on Rojo process: {}", err),
            }

            let info = match self.get_api_rojo() {
                Ok(info) => info,
                Err(err) => {
                    let retry_time_ms =
                        (BASE_DURATION_MS * (i as f32).powf(EXP_BACKOFF_FACTOR)).min(MAX_RETRY_MS);
                    let retry_time = Duration::from_millis(retry_time_ms as u64);

                    log::info!("Server error, retrying in {:?}: {}", retry_time, err);
                    thread::sleep(retry_time);
                    continue;
                }
            };

            log::info!("Got session info: {:?}", info);

            return info;
        }

        panic!(
            "Rojo server did not respond within {:?} ({} tries).",
            STARTUP_TIMEOUT, i
        );
    }

    pub fn get_api_rojo(&self) -> Result<ServerInfoResponse, reqwest::Error> {
        let url = format!("http://localhost:{}/api/rojo", self.port);
        let body = reqwest::blocking::get(url)?.bytes()?;

        Ok(deserialize_msgpack(&body).expect("Server returned malformed response"))
    }

    pub fn get_api_read(&self, id: Ref) -> Result<ReadResponse<'_>, reqwest::Error> {
        let url = format!("http://localhost:{}/api/read/{}", self.port, id);
        let body = reqwest::blocking::get(url)?.bytes()?;

        Ok(deserialize_msgpack(&body).expect("Server returned malformed response"))
    }

    pub fn get_api_socket_packet(
        &self,
        packet_type: SocketPacketType,
        cursor: u32,
    ) -> Result<SocketPacket<'static>, Box<dyn std::error::Error>> {
        let url = format!("ws://localhost:{}/api/socket/{}", self.port, cursor);

        let (mut socket, _response) = connect(url)?;

        // Wait for messages with a timeout
        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err("Timeout waiting for packet from WebSocket".into());
            }

            match socket.read() {
                Ok(Message::Binary(binary)) => {
                    let packet: SocketPacket = deserialize_msgpack(&binary)?;
                    if packet.packet_type != packet_type {
                        continue;
                    }

                    // Close the WebSocket connection now that we got what we were waiting for
                    let _ = socket.close(None);
                    return Ok(packet);
                }
                Ok(Message::Close(_)) => {
                    return Err("WebSocket closed before receiving messages".into());
                }
                Ok(_) => {
                    // Ignore other message types (ping, pong, text)
                    continue;
                }
                Err(hyper_tungstenite::tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // No data available yet, sleep a bit and try again
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }

    pub fn post_api_write(&self, request: &WriteRequest) -> WriteResponse {
        let url = format!("http://localhost:{}/api/write", self.port);
        let body = serialize_msgpack(request).unwrap();
        let response = reqwest::blocking::Client::new()
            .post(url)
            .body(body)
            .send()
            .expect("Failed to send write request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        deserialize_msgpack(&response.bytes().unwrap())
            .expect("Server returned malformed write response")
    }

    pub fn post_api_serialize(
        &self,
        ids: &[Ref],
        session_id: SessionId,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/serialize", self.port);
        let body = serialize_msgpack(&SerializeRequest {
            session_id,
            ids: ids.to_vec(),
        })
        .unwrap();

        client.post(url).body(body).send()
    }

    /// Sends a GET to `/api/rojo` with the given extra request headers and
    /// returns the full response. Used to exercise the Host/Origin allowlist that
    /// guards against DNS rebinding, including asserting that a rejection reveals
    /// nothing about the server.
    pub fn api_rojo_response_with_headers(
        &self,
        headers: &[(&str, &str)],
    ) -> reqwest::blocking::Response {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/rojo", self.port);

        let mut request = client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        request.send().expect("Failed to send request")
    }

    /// Sends a POST to `/api/open/<id>` and returns the response status code.
    /// Used to verify that the local-only gate on `/api/open` admits loopback
    /// peers (the test harness always connects over loopback).
    pub fn api_open_status(&self, id: &str) -> reqwest::StatusCode {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://localhost:{}/api/open/{}", self.port, id);

        client
            .post(url)
            .send()
            .expect("Failed to send request")
            .status()
    }
}

pub fn serialize_msgpack<T: Serialize>(value: T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut serialized = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut serialized)
        .with_human_readable()
        .with_struct_map();

    value.serialize(&mut serializer)?;

    Ok(serialized)
}

pub fn deserialize_msgpack<'a, T: Deserialize<'a>>(
    input: &'a [u8],
) -> Result<T, rmp_serde::decode::Error> {
    let mut deserializer = rmp_serde::Deserializer::new(input).with_human_readable();

    T::deserialize(&mut deserializer)
}

/// Reads the bound port from a child started with `--port 0`. The child retains
/// its listener throughout startup, avoiding any probe-and-release port race.
pub fn read_server_port(output: impl Read + Send + 'static, prefix: &'static str) -> usize {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Some(port) = line.trim().strip_prefix(prefix) {
                if let Ok(port) = port.trim().trim_end_matches('.').parse::<u16>() {
                    let _ = sender.send(usize::from(port));
                }
            }
            // Drain the remaining output so the child never blocks on a full pipe.
            eprintln!("{line}");
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("Server did not report its bound port")
}

#[test]
fn startup_ports_are_read_from_actual_listening_messages() {
    assert_eq!(
        read_server_port(
            std::io::Cursor::new("Starting\n  Port:    12345\nReady\n"),
            "Port:"
        ),
        12345
    );
    assert_eq!(
        read_server_port(
            std::io::Cursor::new("Waiting for Studio on localhost:23456.\n"),
            "Waiting for Studio on localhost:"
        ),
        23456
    );
}

/// Takes a SerializeResponse and creates an XML model out of the response.
///
/// Since the provided structure intentionally includes unredacted referents,
/// some post-processing is done to ensure they don't show up in the model.
pub fn serialize_to_xml_model(response: &SerializeResponse, redactions: &RedactionMap) -> String {
    let mut dom = rbx_binary::from_reader(response.model_contents.as_slice()).unwrap();
    // This makes me realize that maybe we need a `descendants_mut` iter.
    let ref_list: Vec<Ref> = dom.descendants().map(|inst| inst.referent()).collect();
    for referent in ref_list {
        let inst = dom.get_by_ref_mut(referent).unwrap();
        if let Some(id) = redactions.get_id_for_value(&inst.name) {
            inst.name = format!("id-{id}");
        }
    }

    let mut data = Vec::new();
    rbx_xml::to_writer_default(&mut data, &dom, dom.root().children()).unwrap();
    String::from_utf8(data).expect("rbx_xml should never produce invalid utf-8")
}
