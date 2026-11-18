//! Transient, bounded capture jobs for the Studio client of a serve session.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, Context};
use futures::channel::{mpsc, oneshot};
use hyper::{body::HttpBody, Body, Method, Request, Response, StatusCode};
use rbx_dom_weak::types::Variant;
use serde::{Deserialize, Serialize};

use crate::{session_id::SessionId, syncback::from_studio::UploadParts};

use super::interface::ErrorResponse;
use super::util::{deserialize_msgpack, msgpack, msgpack_ok};

pub const PART_SIZE: usize = 1024 * 1024;
const MAX_PARTS: u32 = 256;
const MAX_READERS: usize = 4;
const BODY_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_LIMIT: usize = 4 * 1024 * 1024;
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Place,
    Model,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportRequest {
    pub session_id: SessionId,
    pub scope: Scope,
    pub paths: Vec<String>,
    pub selection: bool,
    pub allow_incomplete: bool,
}

impl ExportRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self.scope {
            Scope::Place if self.selection || !self.paths.is_empty() => {
                bail!("Place exports cannot specify instance paths or a selection.");
            }
            Scope::Model if self.selection != self.paths.is_empty() => {
                bail!("Model exports require either instance paths or --selection, not both.");
            }
            _ => {}
        }
        if self.paths.len() > 128
            || self
                .paths
                .iter()
                .any(|path| path.is_empty() || path.len() > 4096)
        {
            bail!("Specify at most 128 nonempty instance paths of at most 4096 bytes each.");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRequest {
    pub job_id: SessionId,
    pub scope: Scope,
    pub paths: Vec<String>,
    pub selection: bool,
    pub allow_incomplete: bool,
    pub part_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRoot {
    pub name: String,
    pub class_name: String,
    pub service: Option<String>,
    pub descendant_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureService {
    pub name: String,
    pub class_name: String,
    #[serde(deserialize_with = "deserialize_service_properties")]
    pub properties: HashMap<String, Variant>,
}

fn deserialize_service_properties<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<String, Variant>, D::Error> {
    super::interface::deserialize_changed_properties(deserializer).map(|properties| {
        properties
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name.to_string(), value)))
            .collect()
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureManifest {
    pub part_count: u32,
    pub roots: Vec<CaptureRoot>,
    pub services: Vec<CaptureService>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureResult {
    #[serde(with = "serde_bytes")]
    pub model_contents: Vec<u8>,
    pub manifest: CaptureManifest,
}

type CaptureSender = oneshot::Sender<Result<CaptureResult, String>>;

struct Job {
    id: SessionId,
    client: SessionId,
    parts: UploadParts,
    sender: Option<CaptureSender>,
    deadline: tokio::time::Instant,
}

#[derive(Default)]
struct State {
    clients: HashMap<SessionId, mpsc::UnboundedSender<CaptureRequest>>,
    job: Option<Job>,
}

#[derive(Default)]
pub struct ExportJobs(Mutex<State>, AtomicUsize);

struct ExportReader<'a>(&'a AtomicUsize);

impl Drop for ExportReader<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Dropping a socket unregisters it even when its handler returns early.
pub struct ExportConnection {
    jobs: Arc<ExportJobs>,
    id: SessionId,
}

impl Drop for ExportConnection {
    fn drop(&mut self) {
        let mut state = self.jobs.0.lock().unwrap();
        state.clients.remove(&self.id);
        if state.job.as_ref().is_some_and(|job| job.client == self.id) {
            if let Some(sender) = state.job.take().and_then(|job| job.sender) {
                let _ = sender.send(Err("Studio disconnected during export.".into()));
            }
        }
    }
}

/// Cancellation, timeout, and normal completion all release the upload buffers.
struct PendingCapture {
    jobs: Arc<ExportJobs>,
    id: SessionId,
}

impl Drop for PendingCapture {
    fn drop(&mut self) {
        let mut state = self.jobs.0.lock().unwrap();
        if state.job.as_ref().is_some_and(|job| job.id == self.id) {
            state.job = None;
        }
    }
}

impl ExportJobs {
    fn reader(&self) -> Option<ExportReader<'_>> {
        self.1
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_READERS).then_some(active + 1)
            })
            .ok()
            .map(|_| ExportReader(&self.1))
    }

    pub fn connect(
        self: &Arc<Self>,
    ) -> (ExportConnection, mpsc::UnboundedReceiver<CaptureRequest>) {
        let id = SessionId::new();
        let (sender, receiver) = mpsc::unbounded();
        self.0.lock().unwrap().clients.insert(id, sender);
        (
            ExportConnection {
                jobs: Arc::clone(self),
                id,
            },
            receiver,
        )
    }

    fn begin(
        self: &Arc<Self>,
        request: ExportRequest,
    ) -> anyhow::Result<(
        PendingCapture,
        oneshot::Receiver<Result<CaptureResult, String>>,
    )> {
        request.validate()?;
        let mut state = self.0.lock().unwrap();
        if state.job.is_some() {
            bail!("Another Studio export is already in progress.");
        }
        if state.clients.len() != 1 {
            bail!(
                "Export requires exactly one connected Studio client, but found {}. Connect one Studio window to this serve port.",
                state.clients.len()
            );
        }
        let (&client, sender) = state.clients.iter().next().unwrap();
        let id = SessionId::new();
        let (result_sender, receiver) = oneshot::channel();
        sender
            .unbounded_send(CaptureRequest {
                job_id: id,
                scope: request.scope,
                paths: request.paths,
                selection: request.selection,
                allow_incomplete: request.allow_incomplete,
                part_size: PART_SIZE,
            })
            .context("Studio disconnected before export could start")?;
        state.job = Some(Job {
            id,
            client,
            parts: UploadParts::default(),
            sender: Some(result_sender),
            deadline: tokio::time::Instant::now() + CAPTURE_TIMEOUT,
        });
        Ok((
            PendingCapture {
                jobs: Arc::clone(self),
                id,
            },
            receiver,
        ))
    }

    fn with_job<T>(
        &self,
        id: &str,
        action: impl FnOnce(&mut Job) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut state = self.0.lock().unwrap();
        let job = state
            .job
            .as_mut()
            .filter(|job| job.id.to_string() == id && job.sender.is_some())
            .context("The export job is expired, completed, or unknown")?;
        action(job)
    }

    fn insert(&self, id: &str, index: u32, bytes: Vec<u8>) -> anyhow::Result<()> {
        if bytes.len() > PART_SIZE || index >= MAX_PARTS {
            bail!("The export part exceeds the upload limits.");
        }
        self.with_job(id, |job| job.parts.insert(index, bytes))
    }

    fn finish(&self, id: &str, manifest: CaptureManifest) -> anyhow::Result<()> {
        self.with_job(id, |job| {
            if manifest.part_count > MAX_PARTS {
                bail!("The export exceeds the upload limit.");
            }
            let result = match &manifest.error {
                Some(error) => Err(error.clone()),
                None => Ok(CaptureResult {
                    model_contents: job.parts.join(manifest.part_count)?,
                    manifest,
                }),
            };
            job.parts = UploadParts::default();
            let sender = job.sender.take().unwrap();
            let _ = sender.send(result);
            Ok(())
        })
    }
}

async fn read_limited(body: Body, limit: usize) -> anyhow::Result<Vec<u8>> {
    read_until(body, limit, tokio::time::Instant::now() + BODY_TIMEOUT).await
}

async fn read_until(
    mut body: Body,
    limit: usize,
    deadline: tokio::time::Instant,
) -> anyhow::Result<Vec<u8>> {
    let deadline = deadline.min(tokio::time::Instant::now() + BODY_TIMEOUT);
    tokio::time::timeout_at(deadline, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk?;
            if chunk.len() > limit - bytes.len() {
                bail!("The export request exceeds the body size limit.");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .context("The export request body timed out")?
}

pub async fn route(
    jobs: Arc<ExportJobs>,
    session_id: SessionId,
    request: Request<Body>,
) -> Response<Body> {
    let Some(_reader) = jobs.reader() else {
        return msgpack(
            ErrorResponse::bad_request("Too many concurrent export requests."),
            StatusCode::TOO_MANY_REQUESTS,
        );
    };
    match handle(Arc::clone(&jobs), session_id, request).await {
        Ok(response) => response,
        Err(error) => msgpack(
            ErrorResponse::bad_request(format!("{error:#}")),
            StatusCode::BAD_REQUEST,
        ),
    }
}

async fn handle(
    jobs: Arc<ExportJobs>,
    session_id: SessionId,
    request: Request<Body>,
) -> anyhow::Result<Response<Body>> {
    if request.method() != Method::POST {
        bail!("Export endpoints require POST.");
    }
    let path = request.uri().path().to_owned();
    if path == "/api/export" {
        let bytes = read_limited(request.into_body(), REQUEST_LIMIT).await?;
        let request: ExportRequest = deserialize_msgpack(&bytes)?;
        if request.session_id != session_id {
            bail!("The serve session changed. Retry the export.");
        }
        let (_pending, receiver) = jobs.begin(request)?;
        let result = tokio::time::timeout(CAPTURE_TIMEOUT, receiver)
            .await
            .context("Studio export timed out")?
            .context("Studio export was cancelled")?
            .map_err(anyhow::Error::msg)?;
        return Ok(msgpack_ok(result));
    }
    if let Some(argument) = path.strip_prefix("/api/export/part/") {
        let (id, index) = argument
            .split_once('/')
            .context("Expected an export job ID and part index")?;
        let index = index.parse::<u32>().context("Invalid export part index")?;
        // Reject stale jobs before receiving a potentially large body, then recheck on insert.
        let deadline = jobs.with_job(id, |job| Ok(job.deadline))?;
        let bytes = read_until(request.into_body(), PART_SIZE, deadline).await?;
        jobs.insert(id, index, bytes)?;
        return Ok(msgpack_ok(()));
    }
    if let Some(id) = path.strip_prefix("/api/export/finish/") {
        let deadline = jobs.with_job(id, |job| Ok(job.deadline))?;
        let bytes = read_until(request.into_body(), REQUEST_LIMIT, deadline).await?;
        let manifest = deserialize_msgpack(&bytes)?;
        jobs.finish(id, manifest)?;
        return Ok(msgpack_ok(()));
    }
    bail!("Unknown export endpoint.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn request() -> ExportRequest {
        ExportRequest {
            session_id: SessionId::new(),
            scope: Scope::Model,
            paths: vec!["Workspace/Model".into()],
            selection: false,
            allow_incomplete: false,
        }
    }

    fn manifest(parts: u32) -> CaptureManifest {
        CaptureManifest {
            part_count: parts,
            roots: vec![],
            services: vec![],
            warnings: vec![],
            error: None,
        }
    }

    #[test]
    fn service_properties_accept_empty_luau_tables_without_accepting_nonempty_arrays() {
        for properties in [
            serde_json::json!([]),
            serde_json::json!({"Attributes": {"Attributes": []}}),
        ] {
            let value = serde_json::json!({"name": "Workspace", "className": "Workspace", "properties": properties});
            let service: CaptureService =
                deserialize_msgpack(&super::super::util::serialize_msgpack(value).unwrap())
                    .unwrap();
            assert!(
                service.properties.is_empty()
                    || matches!(service.properties.get("Attributes"), Some(Variant::Attributes(attributes)) if attributes.is_empty())
            );
        }
        let value =
            serde_json::json!({"name": "Workspace", "className": "Workspace", "properties": [1]});
        assert!(deserialize_msgpack::<CaptureService>(
            &super::super::util::serialize_msgpack(value).unwrap()
        )
        .is_err());
    }

    #[test]
    fn selectors_have_exactly_one_mode() {
        for scope in [Scope::Model, Scope::Place] {
            for paths in [false, true] {
                for selection in [false, true] {
                    let mut request = request();
                    request.scope = scope;
                    request.paths = if paths {
                        vec!["Workspace/Part".into()]
                    } else {
                        vec![]
                    };
                    request.selection = selection;
                    let valid = match scope {
                        Scope::Model => paths != selection,
                        Scope::Place => !paths && !selection,
                    };
                    assert_eq!(request.validate().is_ok(), valid);
                }
            }
        }
    }

    #[test]
    fn export_requires_exactly_one_client_and_one_job() {
        let jobs = Arc::new(ExportJobs::default());
        assert!(jobs.begin(request()).is_err());
        let (_connection, _messages) = jobs.connect();
        let (other, _messages) = jobs.connect();
        assert!(jobs.begin(request()).is_err());
        drop(other);
        let (pending, _result) = jobs.begin(request()).unwrap();
        assert!(jobs.begin(request()).is_err());
        drop(pending);
        assert!(jobs.begin(request()).is_ok());
    }

    #[tokio::test]
    async fn parts_complete_out_of_order_and_cannot_be_replayed() {
        let jobs = Arc::new(ExportJobs::default());
        let (_connection, mut messages) = jobs.connect();
        let (_pending, result) = jobs.begin(request()).unwrap();
        let id = messages.next().await.unwrap().job_id.to_string();
        jobs.insert(&id, 1, vec![2]).unwrap();
        assert!(jobs.finish(&id, manifest(2)).is_err());
        jobs.insert(&id, 0, vec![1]).unwrap();
        jobs.finish(&id, manifest(2)).unwrap();
        assert_eq!(result.await.unwrap().unwrap().model_contents, vec![1, 2]);
        assert!(jobs.finish(&id, manifest(2)).is_err());
        assert!(jobs.insert(&id, 0, vec![9]).is_err());
    }

    #[tokio::test]
    async fn disconnect_and_cancellation_release_jobs() {
        let jobs = Arc::new(ExportJobs::default());
        let (connection, mut messages) = jobs.connect();
        let (pending, result) = jobs.begin(request()).unwrap();
        let id = messages.next().await.unwrap().job_id.to_string();
        drop(pending);
        assert!(jobs.insert(&id, 0, vec![1]).is_err());
        assert!(result.await.is_err());
        let (_pending, result) = jobs.begin(request()).unwrap();
        drop(connection);
        assert!(result.await.unwrap().unwrap_err().contains("disconnected"));
    }

    #[tokio::test]
    async fn bounded_uploads_and_capture_errors() {
        let jobs = Arc::new(ExportJobs::default());
        let (_connection, mut messages) = jobs.connect();
        let (_pending, result) = jobs.begin(request()).unwrap();
        let id = messages.next().await.unwrap().job_id.to_string();
        assert!(jobs.insert(&id, MAX_PARTS, vec![]).is_err());
        assert!(jobs.insert(&id, 0, vec![0; PART_SIZE + 1]).is_err());
        assert!(jobs.insert("stale", 0, vec![]).is_err());
        let mut failure = manifest(0);
        failure.error = Some("Serialization failed.".into());
        jobs.finish(&id, failure).unwrap();
        assert_eq!(result.await.unwrap().unwrap_err(), "Serialization failed.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_exports_over_http_and_websocket_without_touching_project_files() {
        use super::super::{api, interface::SocketPacket, util::serialize_msgpack};
        use crate::{cli::ExportCommand, serve_session::ServeSession};
        use hyper::service::{make_service_fn, service_fn};
        use hyper_tungstenite::tungstenite::{connect, Message};
        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        use std::convert::Infallible;

        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("default.project.json");
        let project_bytes = br#"{"name":"ExportTest","tree":{"$className":"DataModel"}}"#;
        std::fs::write(&project, project_bytes).unwrap();
        let session =
            Arc::new(ServeSession::new(memofs::Vfs::new_default().unwrap(), &project).unwrap());
        let make_service = make_service_fn(move |connection: &hyper::server::conn::AddrStream| {
            let session = Arc::clone(&session);
            let remote = connection.remote_addr();
            async move {
                Ok::<_, Infallible>(service_fn(move |request| {
                    let session = Arc::clone(&session);
                    async move { Ok::<_, Infallible>(api::call(session, remote, request).await) }
                }))
            }
        });
        let server = hyper::Server::bind(&([127, 0, 0, 1], 0).into()).serve(make_service);
        let address = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(server.with_graceful_shutdown(async {
            let _ = stopped.await;
        }));
        let (ready, ready_receiver) = oneshot::channel();
        let plugin = tokio::task::spawn_blocking(move || {
            let (mut socket, _) = connect(format!("ws://{address}/api/socket/0")).unwrap();
            socket.send(Message::Ping(vec![1])).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Pong(_)));
            ready.send(()).unwrap();
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            for _ in 0..4 {
                let bytes = socket.read().unwrap().into_data();
                let packet: SocketPacket = deserialize_msgpack(&bytes).unwrap();
                let super::super::interface::SocketPacketBody::Export(capture) = packet.body else {
                    panic!("Expected a capture request");
                };
                let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
                dom.insert(
                    dom.root_ref(),
                    InstanceBuilder::new("Folder").with_name("OnlyInStudio"),
                );
                let mut bytes = Vec::new();
                rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
                client
                    .post(format!(
                        "http://{address}/api/export/part/{}/0",
                        capture.job_id
                    ))
                    .body(bytes)
                    .send()
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                let place = capture.scope == Scope::Place;
                let manifest = CaptureManifest {
                    part_count: 1,
                    roots: vec![CaptureRoot {
                        name: "OnlyInStudio".into(),
                        class_name: "Folder".into(),
                        service: place.then(|| "Workspace".into()),
                        descendant_count: 0,
                    }],
                    services: if place {
                        vec![CaptureService {
                            name: "Workspace".into(),
                            class_name: "Workspace".into(),
                            properties: HashMap::new(),
                        }]
                    } else {
                        vec![]
                    },
                    warnings: vec![],
                    error: None,
                };
                client
                    .post(format!(
                        "http://{address}/api/export/finish/{}",
                        capture.job_id
                    ))
                    .body(serialize_msgpack(manifest).unwrap())
                    .send()
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
            let _ = socket.close(None);
        });
        ready_receiver.await.unwrap();
        for extension in ["rbxm", "rbxmx", "rbxl", "rbxlx"] {
            let output = directory.path().join(format!("capture.{extension}"));
            let target = output.clone();
            let place = extension.starts_with("rbxl");
            tokio::task::spawn_blocking(move || {
                ExportCommand {
                    address: Some(address.ip()),
                    port: address.port(),
                    output: target,
                    instances: if place {
                        vec![]
                    } else {
                        vec!["Workspace/OnlyInStudio".into()]
                    },
                    selection: false,
                    allow_incomplete: place,
                }
                .run()
            })
            .await
            .unwrap()
            .unwrap();
            let bytes = std::fs::read(output).unwrap();
            let decoded = if extension.ends_with('x') {
                rbx_xml::from_reader(bytes.as_slice(), rbx_xml::DecodeOptions::new()).unwrap()
            } else {
                rbx_binary::from_reader(bytes.as_slice()).unwrap()
            };
            assert!(decoded
                .descendants()
                .any(|instance| instance.name == "OnlyInStudio"));
        }
        plugin.await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        assert_eq!(std::fs::read(project).unwrap(), project_bytes);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 5);
    }

    #[tokio::test]
    async fn concurrent_body_readers_are_bounded_and_released() {
        let jobs = Arc::new(ExportJobs::default());
        let readers: Vec<_> = (0..MAX_READERS).map(|_| jobs.reader().unwrap()).collect();
        assert!(jobs.reader().is_none());
        let response = route(
            Arc::clone(&jobs),
            SessionId::new(),
            Request::new(Body::empty()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(readers);
        assert!(jobs.reader().is_some());
    }

    #[tokio::test]
    async fn stalled_bodies_stop_at_the_deadline() {
        let (_sender, body) = Body::channel();
        let error = read_until(
            body,
            PART_SIZE,
            tokio::time::Instant::now() + Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn request_bodies_are_bounded() {
        assert!(read_limited(Body::from(vec![0; 4]), 3).await.is_err());
        assert_eq!(
            read_limited(Body::from(vec![0; 3]), 3).await.unwrap().len(),
            3
        );
    }
}
