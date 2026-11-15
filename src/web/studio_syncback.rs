//! The server behind `rojo syncback --from-studio`. It waits for the Studio
//! plugin to upload the open place, then hands the rebuilt place back to the
//! command and shuts down.
//!
//! Only the syncback routes are served. A plugin that tries to connect as if
//! this were `rojo serve` gets an error instead of a session, since a serve
//! session would push the project into Studio.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use futures::channel::oneshot;
use hyper::{
    body::HttpBody as _,
    server::{conn::AddrStream, Server},
    service::{make_service_fn, service_fn},
    Body, Method, Request, Response, StatusCode,
};
use tokio::runtime::Runtime;

use crate::{
    session_id::SessionId,
    syncback::from_studio::{assemble_place, AssembledPlace, StudioService, UploadParts},
    web::{
        interface::{
            ErrorResponse, SyncbackFinishRequest, SyncbackFinishResponse, SyncbackInfoResponse,
            SyncbackStartRequest, SyncbackStartResponse, PROTOCOL_VERSION, SERVER_VERSION,
        },
        origin,
        util::{deserialize_msgpack, msgpack, msgpack_ok},
    },
};

/// The largest part the plugin uploads in one request. Large places are tens of
/// megabytes, so they're split up and uploaded in parallel.
const PART_SIZE: usize = 4 * 1024 * 1024;

struct Shared {
    syncback_id: SessionId,
    project_name: String,
    services: Vec<StudioService>,
    upload: Mutex<Upload>,

    /// Taken by the first upload that finishes successfully.
    place_sender: Mutex<Option<oneshot::Sender<AssembledPlace>>>,
}

/// The finish request only carries the manifest, which lists the top-level
/// children of each service, so it stays far below this.
const FINISH_SIZE: usize = 16 * 1024 * 1024;

/// The parts of the current upload attempt.
///
/// The server numbers attempts as they start, so their order is settled before
/// any part is sent. Requests still in flight from an earlier attempt carry an
/// older number and are rejected without touching the current one, and only one
/// attempt's parts are ever held.
#[derive(Default)]
struct Upload {
    /// The current attempt, or 0 before the first one starts.
    current: u64,
    parts: UploadParts,
}

impl Upload {
    /// Starts a new attempt, replacing the current one, and returns its number.
    fn start(&mut self) -> u64 {
        self.current += 1;
        self.parts = UploadParts::default();
        self.current
    }

    /// Returns the parts for attempt `id`, which must be the current one.
    fn parts_for(&mut self, id: u64) -> anyhow::Result<&mut UploadParts> {
        if self.current == 0 || id != self.current {
            anyhow::bail!("Upload {id} isn't the current upload attempt.");
        }
        Ok(&mut self.parts)
    }
}

/// Serves the syncback routes on `address` until the plugin finishes an
/// upload, then returns the place it uploaded.
///
/// `allowed_hosts` are extra `Host`/`Origin` values to accept, like `rojo serve`
/// takes from the project. `on_listening` is called once the address is bound.
pub fn receive_place(
    address: SocketAddr,
    allowed_hosts: &[String],
    project_name: String,
    services: Vec<StudioService>,
    on_listening: impl FnOnce(SocketAddr),
) -> anyhow::Result<AssembledPlace> {
    let (place_sender, place_receiver) = oneshot::channel();
    let (stop_sender, stop_receiver) = oneshot::channel::<()>();

    let shared = Arc::new(Shared {
        syncback_id: SessionId::new(),
        project_name,
        services,
        upload: Mutex::new(Upload::default()),
        place_sender: Mutex::new(Some(place_sender)),
    });
    let runtime =
        Runtime::new().context("Failed to start the async runtime for the syncback server")?;
    let _guard = runtime.enter();
    let builder = Server::try_bind(&address).with_context(|| {
        format!(
            "Could not start the syncback server on {address}.\n\
             Another Rojo server might already be running, or another program may be using \
             that port.\n\
             You can pick a different port with the --port option."
        )
    })?;
    let address = builder.local_addr();
    let allowed_hosts = origin::allowed_hosts(address.ip(), address.port(), allowed_hosts);

    let make_service = make_service_fn(move |_conn: &AddrStream| {
        let shared = Arc::clone(&shared);
        let allowed_hosts = allowed_hosts.clone();

        async move {
            Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                let shared = Arc::clone(&shared);
                let allowed_hosts = allowed_hosts.clone();

                async move {
                    // The server writes to the project, so it defends against DNS
                    // rebinding the same way `rojo serve` does.
                    if let Some(response) =
                        origin::check_request_origin(&request, allowed_hosts.as_ref())
                    {
                        return Ok::<_, Infallible>(response);
                    }
                    Ok(route(shared, request).await)
                }
            }))
        }
    });

    let server = builder.serve(make_service).with_graceful_shutdown(async {
        let _ = stop_receiver.await;
    });

    on_listening(address);

    runtime.block_on(async move {
        let mut server = tokio::spawn(server);
        tokio::select! {
            result = &mut server => {
                result.context("The syncback server stopped unexpectedly")?
                    .context("The syncback server encountered a fatal error")?;
                anyhow::bail!("The syncback server stopped before Studio finished uploading.");
            }
            place = place_receiver => {
                let place = place.context("The syncback server dropped the uploaded place")?;
                // Graceful shutdown lets the response to the final request finish
                // sending before the server stops.
                let _ = stop_sender.send(());
                server
                    .await
                    .context("The syncback server stopped unexpectedly")?
                    .context("The syncback server encountered a fatal error")?;
                Ok(place)
            }
        }
    })
}

async fn route(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/api/syncback") => msgpack_ok(SyncbackInfoResponse {
            server_version: SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            syncback_id: shared.syncback_id,
            project_name: shared.project_name.clone(),
            services: shared.services.clone(),
            part_size: PART_SIZE,
        }),
        (&Method::POST, "/api/syncback/start") => handle_start(shared, request).await,
        (&Method::POST, "/api/syncback/finish") => handle_finish(shared, request).await,
        (&Method::POST, path) if path.starts_with("/api/syncback/part/") => {
            handle_part(shared, request).await
        }
        _ => msgpack(
            ErrorResponse::not_found(
                "This Rojo server is running `rojo syncback --from-studio`, so it can't be \
                 connected to like `rojo serve`. Run the plugin's syncback instead.",
            ),
            StatusCode::NOT_FOUND,
        ),
    }
}

/// Handles `POST /api/syncback/start`, which begins a new upload attempt and
/// discards any earlier one.
async fn handle_start(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let data = match read_limited(request.into_body(), FINISH_SIZE).await {
        Ok(data) => data,
        Err(err) => return bad_request(format!("Could not read the request. {err}")),
    };
    let request: SyncbackStartRequest = match deserialize_msgpack(&data) {
        Ok(request) => request,
        Err(err) => return bad_request(format!("Invalid body. {err}")),
    };
    if request.syncback_id != shared.syncback_id {
        return bad_request("Wrong syncback ID");
    }

    msgpack_ok(SyncbackStartResponse {
        upload_id: shared.upload.lock().unwrap().start(),
    })
}

/// Handles `POST /api/syncback/part/{syncbackId}/{uploadId}/{index}`, whose body
/// is the raw bytes of one part.
async fn handle_part(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let argument = request.uri().path()["/api/syncback/part/".len()..].to_owned();
    let segments: Vec<_> = argument.split('/').collect();
    let [id, upload_id, index] = segments[..] else {
        return bad_request("Expected /api/syncback/part/{syncbackId}/{uploadId}/{index}");
    };
    if id != shared.syncback_id.to_string() {
        return bad_request("Wrong syncback ID");
    }
    let Ok(upload_id) = upload_id.parse::<u64>() else {
        return bad_request(format!("Malformed upload ID \"{upload_id}\""));
    };
    let Ok(index) = index.parse::<u32>() else {
        return bad_request(format!("Malformed part index \"{index}\""));
    };

    // A stale attempt is turned away before its body is read. It's checked
    // again below, since a new attempt can start while the body arrives.
    if let Err(err) = shared.upload.lock().unwrap().parts_for(upload_id) {
        return bad_request(format!("{err:#}"));
    }

    let data = match read_limited(request.into_body(), PART_SIZE).await {
        Ok(data) => data,
        Err(err) => return bad_request(format!("Could not read part {index}. {err}")),
    };

    let mut upload = shared.upload.lock().unwrap();
    let inserted = upload
        .parts_for(upload_id)
        .and_then(|parts| parts.insert(index, data));
    if let Err(err) = inserted {
        return bad_request(format!("{err:#}"));
    }
    msgpack_ok(())
}

/// Reads a request body, failing as soon as it passes `limit` bytes instead of
/// buffering all of it first.
async fn read_limited(mut body: Body, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        if data.len() + chunk.len() > limit {
            anyhow::bail!("The request body is larger than the limit of {limit} bytes.");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn handle_finish(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let data = match read_limited(request.into_body(), FINISH_SIZE).await {
        Ok(data) => data,
        Err(err) => return bad_request(format!("Could not read the request. {err}")),
    };
    let request: SyncbackFinishRequest = match deserialize_msgpack(&data) {
        Ok(request) => request,
        Err(err) => return bad_request(format!("Invalid body. {err}")),
    };
    if request.syncback_id != shared.syncback_id {
        return bad_request("Wrong syncback ID");
    }

    let joined = shared
        .upload
        .lock()
        .unwrap()
        .parts_for(request.upload_id)
        .and_then(|parts| parts.join(request.part_count));
    let bytes = match joined {
        Ok(bytes) => bytes,
        Err(err) => return bad_request(format!("{err:#}")),
    };

    // Decoding a large place takes about a second, so it runs off the async
    // worker threads.
    let services = shared.services.clone();
    let assembled = tokio::task::spawn_blocking(move || {
        assemble_place(
            &bytes,
            &services,
            &request.manifest,
            request.current_camera.as_deref(),
        )
    })
    .await;
    let place = match assembled {
        Ok(Ok(place)) => place,
        Ok(Err(err)) => return bad_request(format!("{err:#}")),
        Err(err) => {
            return msgpack(
                ErrorResponse::internal_error(format!("Assembling the place failed. {err}")),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
    };

    let response = SyncbackFinishResponse {
        syncback_id: shared.syncback_id,
        instance_count: place.dom.descendants().count() - 1,
        dropped: place.dropped.clone(),
    };

    let Some(sender) = shared.place_sender.lock().unwrap().take() else {
        return bad_request("This syncback already received a place from Studio.");
    };
    if sender.send(place).is_err() {
        return msgpack(
            ErrorResponse::internal_error("The syncback command is no longer waiting"),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    msgpack_ok(response)
}

fn bad_request(details: impl Into<String>) -> Response<Body> {
    msgpack(ErrorResponse::bad_request(details), StatusCode::BAD_REQUEST)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parts_need_a_started_attempt() {
        let mut upload = Upload::default();
        for id in [0, 1, u64::MAX] {
            assert!(upload.parts_for(id).is_err());
        }
    }

    #[test]
    fn a_new_attempt_starts_with_no_parts() {
        let mut upload = Upload::default();
        let a = upload.start();
        upload.parts_for(a).unwrap().insert(5, vec![1]).unwrap();
        let b = upload.start();
        upload.parts_for(b).unwrap().insert(0, vec![2]).unwrap();

        assert_eq!(upload.parts_for(b).unwrap().join(1).unwrap(), vec![2]);
    }

    #[test]
    fn a_delayed_first_part_from_a_replaced_attempt_never_touches_the_retry() {
        let mut upload = Upload::default();
        // Attempt "a" starts, but none of its parts arrive before "b" starts.
        let a = upload.start();
        let b = upload.start();
        upload.parts_for(b).unwrap().insert(0, vec![2]).unwrap();

        // The first part and the finish from "a" finally arrive.
        assert!(upload.parts_for(a).is_err());
        assert_eq!(upload.parts_for(b).unwrap().join(1).unwrap(), vec![2]);
    }

    #[test]
    fn only_the_newest_attempt_is_accepted() {
        let mut upload = Upload::default();
        let ids: Vec<_> = (0..5).map(|_| upload.start()).collect();
        let (newest, older) = ids.split_last().unwrap();
        for id in older {
            assert!(
                upload.parts_for(*id).is_err(),
                "attempt {id} should be stale"
            );
        }
        assert!(upload.parts_for(newest + 1).is_err());
        assert!(upload.parts_for(*newest).is_ok());
    }
}
