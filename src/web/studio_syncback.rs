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
    body,
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
            PROTOCOL_VERSION, SERVER_VERSION,
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
    parts: Mutex<UploadParts>,

    /// Taken by the first upload that finishes successfully.
    place_sender: Mutex<Option<oneshot::Sender<AssembledPlace>>>,
}

/// Serves the syncback routes on `address` until the plugin finishes an
/// upload, then returns the place it uploaded.
///
/// `on_listening` is called once the address is bound.
pub fn receive_place(
    address: SocketAddr,
    project_name: String,
    services: Vec<StudioService>,
    on_listening: impl FnOnce(),
) -> anyhow::Result<AssembledPlace> {
    let (place_sender, place_receiver) = oneshot::channel();
    let (stop_sender, stop_receiver) = oneshot::channel::<()>();

    let shared = Arc::new(Shared {
        syncback_id: SessionId::new(),
        project_name,
        services,
        parts: Mutex::new(UploadParts::default()),
        place_sender: Mutex::new(Some(place_sender)),
    });
    let allowed_hosts = origin::allowed_hosts(address.ip(), address.port(), &[]);

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

    let runtime =
        Runtime::new().context("Failed to start the async runtime for the syncback server")?;
    let _guard = runtime.enter();
    let server = Server::try_bind(&address)
        .with_context(|| {
            format!(
                "Could not start the syncback server on {address}.\n\
                 Another Rojo server might already be running, or another program may be using \
                 that port.\n\
                 You can pick a different port with the --port option."
            )
        })?
        .serve(make_service)
        .with_graceful_shutdown(async {
            let _ = stop_receiver.await;
        });

    on_listening();

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

/// Handles `POST /api/syncback/part/{syncbackId}/{index}`, whose body is the raw
/// bytes of one part.
async fn handle_part(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let argument = &request.uri().path()["/api/syncback/part/".len()..];
    let Some((id, index)) = argument.split_once('/') else {
        return bad_request("Expected /api/syncback/part/{syncbackId}/{index}");
    };
    if id != shared.syncback_id.to_string() {
        return bad_request("Wrong syncback ID");
    }
    let Ok(index) = index.parse::<u32>() else {
        return bad_request(format!("Malformed part index \"{index}\""));
    };

    let data = match body::to_bytes(request.into_body()).await {
        Ok(data) => data,
        Err(err) => return bad_request(format!("Could not read part {index}. {err}")),
    };
    if data.len() > PART_SIZE {
        return bad_request(format!(
            "Part {index} is {} bytes, but parts can be at most {PART_SIZE} bytes.",
            data.len()
        ));
    }

    shared.parts.lock().unwrap().insert(index, data.to_vec());
    msgpack_ok(())
}

async fn handle_finish(shared: Arc<Shared>, request: Request<Body>) -> Response<Body> {
    let data = match body::to_bytes(request.into_body()).await {
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

    let bytes = match shared.parts.lock().unwrap().join(request.part_count) {
        Ok(bytes) => bytes,
        Err(err) => return bad_request(format!("{err:#}")),
    };

    // Decoding a large place takes about a second, so it runs off the async
    // worker threads.
    let services = shared.services.clone();
    let assembled = tokio::task::spawn_blocking(move || {
        assemble_place(&bytes, &services, &request.manifest, request.current_camera)
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
