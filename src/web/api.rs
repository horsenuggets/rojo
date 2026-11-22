//! Defines Rojo's HTTP API, all under /api. These endpoints generally return
//! JSON.

use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use futures::{sink::SinkExt, stream::StreamExt};
use hyper::{body, Body, Method, Request, Response, StatusCode};
use hyper_tungstenite::{is_upgrade_request, tungstenite::Message, upgrade, HyperWebsocket};
use opener::OpenError;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, InstanceBuilder, UstrMap, WeakDom,
};

use crate::{
    change_processor::TreeMutation,
    rojo_ref::merge_link_attributes,
    serve_session::ServeSession,
    snapshot::{InstanceWithMeta, PatchSet, PatchUpdate},
    web::{
        interface::{
            ErrorResponse, Instance, MessagesPacket, OpenResponse, ReadResponse,
            ServerInfoResponse, SocketPacket, SocketPacketBody, SocketPacketType, SubscribeMessage,
            WriteError, WriteRequest, WriteResponse, PROTOCOL_VERSION, SERVER_VERSION,
        },
        origin::canonical,
        util::{deserialize_msgpack, msgpack, msgpack_ok, serialize_msgpack},
    },
    web_api::{
        InstanceUpdate, RefPatchRequest, RefPatchResponse, SerializeRequest, SerializeResponse,
    },
};

const MAX_DISCARD_BODY_BYTES: usize = 1024;
static NEXT_WRITE_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointCreatedResponse {
    session_id: crate::SessionId,
    checkpoint_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointDiscardRequest {
    session_id: crate::SessionId,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointListResponse {
    session_id: crate::SessionId,
    checkpoints: Vec<crate::source_checkpoints::CheckpointMetadata>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointReadResponse {
    session_id: crate::SessionId,
    checkpoint: crate::source_checkpoints::Checkpoint,
}

pub async fn call(
    serve_session: Arc<ServeSession>,
    remote_addr: SocketAddr,
    mut request: Request<Body>,
) -> Response<Body> {
    let service = ApiService::new(serve_session, remote_addr);

    match (request.method(), request.uri().path()) {
        (_, path) if path == "/api/export" || path.starts_with("/api/export/") => {
            super::export::route(
                service.serve_session.export_jobs(),
                service.serve_session.session_id(),
                request,
            )
            .await
        }
        (_, path)
            if path == "/api/source-checkpoints"
                || path.starts_with("/api/source-checkpoints/") =>
        {
            service.handle_source_checkpoints(request).await
        }
        (&Method::GET, "/api/rojo") => service.handle_api_rojo().await,
        (&Method::GET, path) if path.starts_with("/api/read/") => {
            service.handle_api_read(request).await
        }
        (&Method::GET, path) if path.starts_with("/api/socket/") => {
            if is_upgrade_request(&request) {
                service.handle_api_socket(&mut request).await
            } else {
                msgpack(
                    ErrorResponse::bad_request(
                        "/api/socket must be called as a websocket upgrade request",
                    ),
                    StatusCode::BAD_REQUEST,
                )
            }
        }
        (&Method::POST, "/api/serialize") => service.handle_api_serialize(request).await,
        (&Method::POST, "/api/ref-patch") => service.handle_api_ref_patch(request).await,

        (&Method::POST, path) if path.starts_with("/api/open/") => {
            service.handle_api_open(request).await
        }
        (&Method::POST, "/api/write") => service.handle_api_write(request).await,

        (_method, path) => msgpack(
            ErrorResponse::not_found(format!("Route not found: {}", path)),
            StatusCode::NOT_FOUND,
        ),
    }
}

// Keep diagnostics scalar-only. Debug-printing a response leaks script text, values,
// paths and error details, and formats potentially large conflict snapshots.
fn write_response_summary(response: &WriteResponse) -> String {
    format!(
        "result cursor {}, accepted additions {}, removals {}, updates {}, properties {}, \
         conflict additions {}, removals {}, updates {}, properties {}, errors {}",
        response.message_cursor,
        response.accepted.added_instances.len(),
        response.accepted.removed_instances.len(),
        response.accepted.updated_instances.len(),
        response
            .accepted
            .updated_instances
            .iter()
            .map(|update| update.changed_properties.len())
            .sum::<usize>(),
        response.conflicts.added_instances.len(),
        response.conflicts.removed_instances.len(),
        response.conflicts.updated_instances.len(),
        response
            .conflicts
            .updated_instances
            .iter()
            .map(|update| update.changed_properties.len())
            .sum::<usize>(),
        response.errors.len(),
    )
}

pub struct ApiService {
    serve_session: Arc<ServeSession>,
    remote_addr: SocketAddr,
}

impl ApiService {
    pub fn new(serve_session: Arc<ServeSession>, remote_addr: SocketAddr) -> Self {
        ApiService {
            serve_session,
            remote_addr,
        }
    }

    async fn handle_source_checkpoints(&self, mut request: Request<Body>) -> Response<Body> {
        use super::interface::SourceCheckpointRequest;
        use crate::source_checkpoints::{
            valid_checkpoint_id, Checkpoint, CheckpointMetadata, Provenance, Store, MAX_BODY_BYTES,
            SCHEMA_VERSION,
        };
        use hyper::body::HttpBody;
        let session_id = self.serve_session.session_id();
        let path = request.uri().path().to_owned();
        let argument = path.strip_prefix("/api/source-checkpoints/");
        if let Some(id) = argument {
            if !valid_checkpoint_id(id) {
                return msgpack(
                    ErrorResponse::bad_request("Malformed checkpoint ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
        }
        let failure = |error: anyhow::Error| {
            msgpack(
            ErrorResponse::internal_error(format!("Source recovery failed. {error:#}. Preserve editor Source and retry before applying a replacement")),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        };
        if request.method() == Method::POST && argument.is_none() {
            let json = request
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .is_some_and(|value| {
                    value
                        .to_str()
                        .is_ok_and(|value| value.split(';').next() == Some("application/json"))
                });
            let mut body = request.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        return msgpack(
                            ErrorResponse::bad_request("Cannot read checkpoint request body"),
                            StatusCode::BAD_REQUEST,
                        )
                    }
                };
                if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                    return msgpack(ErrorResponse::bad_request("Checkpoint request exceeds the 8 MiB body limit. Copy editor Source to a safe file before continuing"), StatusCode::PAYLOAD_TOO_LARGE);
                }
                bytes.extend_from_slice(&chunk);
            }
            let decoded: anyhow::Result<SourceCheckpointRequest> = if json {
                serde_json::from_slice(&bytes).map_err(Into::into)
            } else {
                deserialize_msgpack(&bytes).map_err(Into::into)
            };
            let decoded = match decoded {
                Ok(decoded) => decoded,
                // Parser diagnostics can contain source fragments. Never return or log them.
                Err(_) => {
                    return msgpack(
                        ErrorResponse::bad_request("Invalid checkpoint request body"),
                        StatusCode::BAD_REQUEST,
                    )
                }
            };
            if decoded.session_id != session_id {
                return msgpack(
                    ErrorResponse::bad_request("Wrong session ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
            // Capture path, transform, source, and queue revision together. Writes retain
            // this same lock through publication, so no independent filesystem event can
            // be mistaken for the baseline of this checkpoint.
            let tree = self.serve_session.tree();
            let captured_cursor = self.serve_session.message_queue().cursor();
            if decoded.message_cursor > captured_cursor {
                return msgpack(
                    ErrorResponse::bad_request(
                        "Checkpoint messageCursor is ahead of the server queue",
                    ),
                    StatusCode::BAD_REQUEST,
                );
            }
            let live = crate::source_checkpoints::SourceDescriptor::capture(&tree, decoded.id);
            // A live script always wins, including a now-read-only incarnation. Only a
            // structurally retired Source can supply fallback authority for this exact ID.
            let descriptor = match &live {
                Ok(source) => source,
                Err(_) => match tree.retired_source(decoded.id).filter(|_| {
                    tree.get_instance(decoded.id).is_none_or(|instance| !matches!(
                        instance.class_name().as_str(), "Script" | "LocalScript" | "ModuleScript"
                    ))
                }) {
                    Some(source) => source,
                    None => return msgpack(ErrorResponse::bad_request(
                        "Checkpoint target is unavailable or its retired Source expired. Preserve editor Source and copy it to a safe file before continuing"
                    ), StatusCode::BAD_REQUEST),
                },
            };
            let project = self.serve_session.current_project();
            match descriptor.validate(&project, &tree, self.serve_session.vfs()) {
                Ok(true) => {}
                Ok(false) => return msgpack(
                    ErrorResponse::bad_request(
                        "Checkpoint target is not writable managed Source. Preserve editor Source",
                    ),
                    StatusCode::BAD_REQUEST,
                ),
                Err(error) => return failure(error),
            }
            let identity = self.serve_session.checkpoint_identity();
            let record = Checkpoint {
                metadata: CheckpointMetadata {
                    checkpoint_id: String::new(),
                    id: decoded.id,
                    name: descriptor.name.clone(),
                    source_path: descriptor.source_path.clone(),
                    created: 0,
                    provenance: Provenance {
                        schema_version: SCHEMA_VERSION,
                        project_identity: identity.to_path_buf(),
                        session_id,
                        message_cursor: decoded.message_cursor,
                        captured_cursor,
                    },
                },
                source: decoded.source,
                expected_source: decoded.expected_source,
                canonical_source: descriptor.canonical_source.clone(),
                source_transform: descriptor.source_transform.clone(),
            };
            // Release authority capture before even creating/opening the durable namespace.
            drop(tree);
            let store = match Store::open_identity(identity) {
                Ok(store) => store,
                Err(error) => return failure(error),
            };
            return match store.create(record) {
                Ok(checkpoint_id) => msgpack_ok(CheckpointCreatedResponse {
                    session_id,
                    checkpoint_id,
                }),
                Err(error) => failure(error),
            };
        }
        if request.method() != Method::GET
            && !(request.method() == Method::DELETE && argument.is_some())
        {
            return msgpack(
                ErrorResponse::not_found("Unsupported source checkpoint route"),
                StatusCode::NOT_FOUND,
            );
        }
        if request.method() == Method::DELETE {
            let json = request
                .headers()
                .get(hyper::header::CONTENT_TYPE)
                .is_some_and(|value| {
                    value
                        .to_str()
                        .is_ok_and(|value| value.split(';').next() == Some("application/json"))
                });
            let mut bytes = Vec::new();
            while let Some(chunk) = request.body_mut().data().await {
                let Ok(chunk) = chunk else {
                    return msgpack(
                        ErrorResponse::bad_request("Cannot read checkpoint discard request"),
                        StatusCode::BAD_REQUEST,
                    );
                };
                if bytes.len().saturating_add(chunk.len()) > MAX_DISCARD_BODY_BYTES {
                    return msgpack(
                        ErrorResponse::bad_request(
                            "Checkpoint discard request exceeds its body limit",
                        ),
                        StatusCode::PAYLOAD_TOO_LARGE,
                    );
                }
                bytes.extend_from_slice(&chunk);
            }
            let decoded: anyhow::Result<CheckpointDiscardRequest> = if json {
                serde_json::from_slice(&bytes).map_err(Into::into)
            } else {
                deserialize_msgpack(&bytes).map_err(Into::into)
            };
            if !decoded.is_ok_and(|body| body.session_id == session_id) {
                return msgpack(
                    ErrorResponse::bad_request(
                        "Checkpoint discard requires the current session ID",
                    ),
                    StatusCode::BAD_REQUEST,
                );
            }
        }
        let store = match Store::open_identity(self.serve_session.checkpoint_identity()) {
            Ok(store) => store,
            Err(error) => return failure(error),
        };
        match (request.method(), argument) {
            (&Method::GET, None) => match store.list() {
                Ok(checkpoints) => msgpack_ok(CheckpointListResponse {
                    session_id,
                    checkpoints,
                }),
                Err(error) => failure(error),
            },
            (&Method::GET, Some(id)) => match store.read(id) {
                Ok(record) => msgpack_ok(CheckpointReadResponse {
                    session_id,
                    checkpoint: record,
                }),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    msgpack(
                        ErrorResponse::not_found("Checkpoint not found"),
                        StatusCode::NOT_FOUND,
                    )
                }
                Err(error) => failure(error),
            },
            (&Method::DELETE, Some(id)) => match store.discard(id) {
                Ok(()) => msgpack_ok(CheckpointCreatedResponse {
                    session_id,
                    checkpoint_id: id.to_owned(),
                }),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    msgpack(
                        ErrorResponse::not_found("Checkpoint not found"),
                        StatusCode::NOT_FOUND,
                    )
                }
                Err(error) => failure(error),
            },
            _ => unreachable!(),
        }
    }

    /// Get a summary of information about the server
    async fn handle_api_rojo(&self) -> Response<Body> {
        let tree = self.serve_session.tree();
        let root_instance_id = tree.get_root_id();

        msgpack_ok(&ServerInfoResponse {
            server_version: SERVER_VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            session_id: self.serve_session.session_id(),
            project_name: self.serve_session.project_name().to_owned(),
            expected_place_ids: self.serve_session.serve_place_ids().cloned(),
            unexpected_place_ids: self.serve_session.blocked_place_ids().cloned(),
            place_id: self.serve_session.place_id(),
            game_id: self.serve_session.game_id(),
            root_instance_id,
        })
    }

    /// Handle WebSocket upgrade for real-time message streaming
    async fn handle_api_socket(&self, request: &mut Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/socket/".len()..];
        let input_cursor: u32 = match argument.parse() {
            Ok(v) => v,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Malformed message cursor: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Upgrade the connection to WebSocket
        let (response, websocket) = match upgrade(request, None) {
            Ok(result) => result,
            Err(err) => {
                return msgpack(
                    ErrorResponse::internal_error(format!("WebSocket upgrade failed: {}", err)),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        let serve_session = Arc::clone(&self.serve_session);

        // Spawn a task to handle the WebSocket connection
        tokio::spawn(async move {
            if let Err(e) =
                handle_websocket_subscription(serve_session, websocket, input_cursor).await
            {
                log::error!("Error in websocket subscription: {}", e);
            }
        });

        response
    }

    async fn handle_api_write(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let tree_mutation_sender = self.serve_session.tree_mutation_sender();

        let diagnostics = log::log_enabled!(log::Level::Debug);
        let started = diagnostics.then(Instant::now);
        let diagnostic_id =
            diagnostics.then(|| NEXT_WRITE_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed));
        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: WriteRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let submitted_cursor = request.message_cursor;
        let authority = request.authority;
        if let Some(diagnostic_id) = diagnostic_id {
            log::debug!(
                "Write {diagnostic_id} submitted. Session {session_id}, cursor {submitted_cursor:?}, authority \
                 {authority:?}, bytes {}, queued {}, additions {}, removals {}, updates {}, properties {}.",
                body.len(),
                tree_mutation_sender.len(),
                request.added.len(),
                request.removed.len(),
                request.updated.len(),
                request.updated.iter().map(|update| update.changed_properties.len()).sum::<usize>(),
            );
        }
        let mut errors: Vec<_> = request
            .updated
            .iter()
            .filter(|update| update.changed_metadata.is_some())
            .map(|update| WriteError {
                kind: None,
                id: update.id,
                property: Some("metadata".into()),
                message: "Changing metadata through two-way sync is not supported".into(),
            })
            .collect();
        let updated_instances = {
            // Studio never receives link attributes, so its attribute edits are
            // completed with the ones the instance already has before persisting.
            let tree = self.serve_session.tree();
            request
                .updated
                .into_iter()
                .map(|update| {
                    let existing = tree.get_instance(update.id);
                    let mut changed_properties = update.changed_properties;
                    if let Some(Some(studio)) = changed_properties.get_mut(&ustr("Attributes")) {
                        let current = existing
                            .and_then(|instance| instance.properties().get(&ustr("Attributes")));
                        *studio = merge_link_attributes(studio, current);
                    }
                    PatchUpdate {
                        id: update.id,
                        changed_class_name: update.changed_class_name,
                        changed_parent: update.changed_parent,
                        changed_name: update.changed_name,
                        changed_properties,
                        changed_metadata: None,
                    }
                })
                .collect()
        };

        let (result_sender, result_receiver) = crossbeam_channel::bounded(1);
        let submitted = diagnostics.then(Instant::now);
        tree_mutation_sender
            .send(TreeMutation {
                operation_id: request.operation_id.clone(),
                predecessor: request.predecessor,
                source_baselines: request.source_baselines,
                message_cursor: request.message_cursor,
                authority: request.authority,
                patch_set: PatchSet {
                    removed_instances: request.removed,
                    added_instances: Vec::new(),
                    updated_instances,
                },
                additions: request.added,
                result_sender,
            })
            .unwrap();
        let ((accepted, conflicts, write_errors), message_cursor, acknowledgement) =
            match tokio::task::spawn_blocking(move || result_receiver.recv()).await {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => {
                    return msgpack(
                        ErrorResponse::internal_error(format!("Write processing failed: {err}")),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
                Err(err) => {
                    return msgpack(
                        ErrorResponse::internal_error(format!("Write processing failed: {err}")),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

        errors.extend(write_errors);
        let acknowledgement = if errors.is_empty() {
            acknowledgement
        } else {
            None
        };
        let response = WriteResponse {
            acknowledgement,
            operation_id: request.operation_id,
            session_id,
            message_cursor,
            accepted,
            conflicts,
            errors,
        };
        if let (Some(diagnostic_id), Some(started), Some(submitted)) =
            (diagnostic_id, started, submitted)
        {
            log::debug!(
                "Write {diagnostic_id} completed. Session {session_id}, submitted cursor {submitted_cursor:?}, \
                 {}, receive-to-result {:.3} ms, queue-and-processing {:.3} ms.",
                write_response_summary(&response),
                started.elapsed().as_secs_f64() * 1000.0,
                submitted.elapsed().as_secs_f64() * 1000.0,
            );
        }
        msgpack_ok(response)
    }

    async fn handle_api_read(&self, request: Request<Body>) -> Response<Body> {
        let argument = &request.uri().path()["/api/read/".len()..];
        let requested_ids: Result<Vec<Ref>, _> = argument.split(',').map(Ref::from_str).collect();

        let requested_ids = match requested_ids {
            Ok(ids) => ids,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Malformed ID list"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Writers retain the tree lock until publishing the matching queue revision.
        let tree = self.serve_session.tree();
        let message_cursor = self.serve_session.message_queue().cursor();

        let mut instances = HashMap::new();

        for id in requested_ids {
            if let Some(instance) = tree.get_instance(id) {
                instances.insert(id, Instance::from_rojo_instance(instance));

                for descendant in tree.descendants(id) {
                    instances.insert(descendant.id(), Instance::from_rojo_instance(descendant));
                }
            }
        }

        msgpack_ok(ReadResponse {
            session_id: self.serve_session.session_id(),
            message_cursor,
            instances,
        })
    }

    /// Accepts a list of IDs and returns them serialized as a binary model.
    /// The model is sent in a schema that causes Roblox to deserialize it as
    /// a Luau `buffer`.
    ///
    /// The returned model is a folder that contains ObjectValues with names
    /// that correspond to the requested Instances. These values have their
    /// `Value` property set to point to the requested Instance.
    async fn handle_api_serialize(&self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: SerializeRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut response_dom = WeakDom::new(InstanceBuilder::new("Folder"));

        let tree = self.serve_session.tree();
        for id in &request.ids {
            if let Some(instance) = tree.get_instance(*id) {
                let clone = response_dom.insert(
                    Ref::none(),
                    InstanceBuilder::new(instance.class_name())
                        .with_name(instance.name())
                        .with_properties(instance.properties().clone()),
                );
                let object_value = response_dom.insert(
                    response_dom.root_ref(),
                    InstanceBuilder::new("ObjectValue")
                        .with_name(id.to_string())
                        .with_property("Value", clone),
                );

                let mut child_ref = clone;
                if let Some(parent_class) = parent_requirements(&instance.class_name()) {
                    child_ref =
                        response_dom.insert(object_value, InstanceBuilder::new(parent_class));
                    response_dom.transfer_within(clone, child_ref);
                }

                response_dom.transfer_within(child_ref, object_value);
            } else {
                return msgpack(
                    ErrorResponse::bad_request(format!("provided id {id} is not in the tree")),
                    StatusCode::BAD_REQUEST,
                );
            }
        }
        drop(tree);

        let mut source = Vec::new();
        rbx_binary::to_writer(&mut source, &response_dom, &[response_dom.root_ref()]).unwrap();

        msgpack_ok(SerializeResponse {
            session_id: self.serve_session.session_id(),
            model_contents: source,
        })
    }

    /// Returns a list of all referent properties that point towards the
    /// provided IDs. Used because the plugin does not store a RojoTree,
    /// and referent properties need to be updated after the serialize
    /// endpoint is used.
    async fn handle_api_ref_patch(self, request: Request<Body>) -> Response<Body> {
        let session_id = self.serve_session.session_id();
        let body = body::to_bytes(request.into_body()).await.unwrap();

        let request: RefPatchRequest = match deserialize_msgpack(&body) {
            Ok(request) => request,
            Err(err) => {
                return msgpack(
                    ErrorResponse::bad_request(format!("Invalid body: {}", err)),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        if request.session_id != session_id {
            return msgpack(
                ErrorResponse::bad_request("Wrong session ID"),
                StatusCode::BAD_REQUEST,
            );
        }

        let mut instance_updates: HashMap<Ref, InstanceUpdate> = HashMap::new();

        let tree = self.serve_session.tree();
        for instance in tree.descendants(tree.get_root_id()) {
            for (prop_name, prop_value) in instance.properties() {
                let Variant::Ref(prop_value) = prop_value else {
                    continue;
                };
                if let Some(target_id) = request.ids.get(prop_value) {
                    let instance_id = instance.id();
                    let update =
                        instance_updates
                            .entry(instance_id)
                            .or_insert_with(|| InstanceUpdate {
                                id: instance_id,
                                changed_class_name: None,
                                changed_parent: None,
                                changed_name: None,
                                changed_metadata: None,
                                changed_properties: UstrMap::default(),
                            });
                    update
                        .changed_properties
                        .insert(*prop_name, Some(Variant::Ref(*target_id)));
                }
            }
        }

        msgpack_ok(RefPatchResponse {
            session_id: self.serve_session.session_id(),
            patch: SubscribeMessage {
                added: HashMap::new(),
                removed: Vec::new(),
                updated: instance_updates.into_values().collect(),
                source_operations: HashMap::new(),
            },
        })
    }

    /// Open a script with the given ID in the user's default text editor.
    async fn handle_api_open(&self, request: Request<Body>) -> Response<Body> {
        // Opening a file launches a local program, so it must never be reachable
        // by a remote client even when the server is bound to an exposed address.
        //
        // `remote_addr` is the immediate peer, which is the best locality signal
        // we have: the legitimate caller is a sandboxed Roblox plugin whose only
        // credential is being able to reach the port, so there is no secret to
        // authenticate it with. A connection forwarded over loopback by an
        // SSH/Tailscale tunnel or a local reverse proxy therefore appears local
        // and is allowed. That is delegated trust rather than a bypass: by
        // standing up that tunnel or proxy the user has decided the remote end is
        // trusted, and reachability is bounded by that hop's own authentication
        // (e.g. SSH keys or Tailscale ACLs). This gate only stops direct,
        // unauthenticated peers.
        //
        // An IPv4 client reaching a dual-stack (`::`) bind appears as an
        // IPv4-mapped IPv6 peer (`::ffff:127.0.0.1`), so canonicalize to the bare
        // IPv4 form before the loopback test, matching `origin`'s handling.
        if !canonical(self.remote_addr.ip()).is_loopback() {
            return msgpack(
                ErrorResponse::forbidden("/api/open is only available to local clients"),
                StatusCode::FORBIDDEN,
            );
        }

        let argument = &request.uri().path()["/api/open/".len()..];
        let requested_id = match Ref::from_str(argument) {
            Ok(id) => id,
            Err(_) => {
                return msgpack(
                    ErrorResponse::bad_request("Invalid instance ID"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        let tree = self.serve_session.tree();

        let instance = match tree.get_instance(requested_id) {
            Some(instance) => instance,
            None => {
                return msgpack(
                    ErrorResponse::bad_request("Instance not found"),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        let script_path = match pick_script_path(instance) {
            Some(path) => path,
            None => {
                return msgpack(
                    ErrorResponse::bad_request(
                        "No appropriate file could be found to open this script",
                    ),
                    StatusCode::NOT_FOUND,
                );
            }
        };

        match opener::open(&script_path) {
            Ok(()) => {}
            Err(error) => match error {
                OpenError::Io(io_error) => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            "Attempting to open {} failed because of the following io error: {}",
                            script_path.display(),
                            io_error
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
                OpenError::ExitStatus {
                    cmd,
                    status,
                    stderr,
                } => {
                    return msgpack(
                        ErrorResponse::internal_error(format!(
                            r#"The command '{}' to open '{}' failed with the error code '{}'.
                            Error logs:
                            {}"#,
                            cmd,
                            script_path.display(),
                            status,
                            stderr
                        )),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
            },
        };

        msgpack_ok(OpenResponse {
            session_id: self.serve_session.session_id(),
        })
    }
}

/// If this instance is represented by a script, try to find the correct .lua or .luau
/// file to open to edit it.
fn pick_script_path(instance: InstanceWithMeta<'_>) -> Option<PathBuf> {
    match instance.class_name().as_str() {
        "Script" | "LocalScript" | "ModuleScript" => {}
        _ => return None,
    }

    // Pick the first listed relevant path that has an extension of .lua or .luau that
    // exists.
    instance
        .metadata()
        .relevant_paths
        .iter()
        .find(|path| {
            // We should only ever open Lua or Luau files to be safe.
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("lua") => {}
                Some("luau") => {}
                _ => return false,
            }

            fs::metadata(path)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        })
        .map(|path| path.to_owned())
}

/// Handle WebSocket connection for streaming subscription messages
async fn handle_websocket_subscription(
    serve_session: Arc<ServeSession>,
    websocket: HyperWebsocket,
    input_cursor: u32,
) -> anyhow::Result<()> {
    let mut websocket = websocket.await?;
    let (_export_connection, mut export_requests) = serve_session.export_jobs().connect();

    let session_id = serve_session.session_id();
    let tree_handle = serve_session.tree_handle();
    let message_queue = serve_session.message_queue();

    log::debug!(
        "WebSocket subscription established for session {}",
        session_id
    );

    // Now continuously listen for new messages using select to handle both incoming messages
    // and WebSocket control messages concurrently
    let mut cursor = input_cursor;
    loop {
        let receiver = message_queue.subscribe(cursor);

        tokio::select! {
            Some(capture) = export_requests.next() => {
                let packet = SocketPacket {
                    session_id,
                    packet_type: SocketPacketType::Export,
                    body: SocketPacketBody::Export(capture),
                };
                if websocket.send(Message::Binary(serialize_msgpack(packet)?)).await.is_err() {
                    break;
                }
            }
            // Handle new messages from the message queue
            result = receiver => {
                match result {
                    Ok((new_cursor, messages)) => {
                        if !messages.is_empty() {
                            let msgpack_message = {
                                let tree = tree_handle.lock().unwrap();
                                let api_messages = messages
                                    .into_iter()
                                    .map(|patch| SubscribeMessage::from_patch_update(&tree, patch))
                                    .collect();

                                let response = SocketPacket {
                                    session_id,
                                    packet_type: SocketPacketType::Messages,
                                    body: SocketPacketBody::Messages(MessagesPacket {
                                        message_cursor: new_cursor,
                                        messages: api_messages,
                                    }),
                                };

                                serialize_msgpack(response)?
                            };

                            log::debug!("Sending batch of messages over WebSocket subscription");

                            if websocket.send(Message::Binary(msgpack_message)).await.is_err() {
                                // Client disconnected
                                log::debug!("WebSocket subscription closed by client");
                                break;
                            }
                            cursor = new_cursor;
                        }
                    }
                    Err(_) => {
                        // Message queue disconnected
                        log::debug!("Message queue disconnected; closing WebSocket subscription");
                        let _ = websocket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }

            // Handle incoming WebSocket messages (ping/pong/close)
            msg = websocket.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) => {
                        log::debug!("WebSocket subscription closed by client");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // tungstenite handles pong automatically
                        log::debug!("Received ping: {:?}", data);
                    }
                    Some(Ok(Message::Pong(data))) => {
                        log::debug!("Received pong: {:?}", data);
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        // Ignore text/binary messages from client for subscription endpoint
                        // TODO: Use this for bidirectional sync or requesting fallbacks?
                        log::debug!("Ignoring message from client since we don't use it for anything yet: {:?}", msg);
                    }
                    Some(Ok(Message::Frame(_))) => {
                        // This should never happen according to tungstenite docs
                        unreachable!();
                    }
                    Some(Err(e)) => {
                        log::error!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        // WebSocket stream ended
                        log::debug!("WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Certain Instances MUST be a child of specific classes. This function
/// tracks that information for the Serialize endpoint.
///
/// If a parent requirement exists, it will be returned.
/// Otherwise returns `None`.
fn parent_requirements(class: &str) -> Option<&str> {
    Some(match class {
        "Attachment" | "Bone" => "Part",
        "Animator" => "Humanoid",
        "BaseWrap" | "WrapLayer" | "WrapTarget" | "WrapDeformer" => "MeshPart",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_id::SessionId;

    fn recovery_session() -> (tempfile::TempDir, Arc<ServeSession>, Ref) {
        recovery_session_with_rules(serde_json::json!({}))
    }

    fn recovery_session_with_rules(
        rules: serde_json::Value,
    ) -> (tempfile::TempDir, Arc<ServeSession>, Ref) {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/Editor.luau"), "return 'disk'\n").unwrap();
        fs::write(directory.path().join("Mapped.luau"), "return 'mapped'\n").unwrap();
        let project = directory.path().join("default.project.json");
        fs::write(&project, serde_json::to_vec(&serde_json::json!({"name":"Recovery", "syncbackRules": rules,
            "tree":{"$className":"Folder","$path":"src","Mapped":{"$path":"Mapped.luau"},"Part":{"$className":"Part"}}})).unwrap()).unwrap();
        let session =
            Arc::new(ServeSession::new(memofs::Vfs::new_default().unwrap(), &project).unwrap());
        let tree = session.tree();
        let id = tree
            .descendants(tree.get_root_id())
            .find(|instance| instance.name() == "Editor")
            .unwrap()
            .id();
        drop(tree);
        (directory, session, id)
    }

    async fn recovery_call(
        session: Arc<ServeSession>,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let created = method == Method::POST || method == Method::DELETE;
        let listed = path == "/api/source-checkpoints";
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = call(session, "127.0.0.1:1".parse().unwrap(), request).await;
        let status = response.status();
        let bytes = body::to_bytes(response.into_body()).await.unwrap();
        let value = if status != StatusCode::OK {
            rmp_serde::from_slice(&bytes).unwrap()
        } else if created {
            serde_json::to_value(deserialize_msgpack::<CheckpointCreatedResponse>(&bytes).unwrap())
                .unwrap()
        } else if listed {
            serde_json::to_value(deserialize_msgpack::<CheckpointListResponse>(&bytes).unwrap())
                .unwrap()
        } else {
            serde_json::to_value(deserialize_msgpack::<CheckpointReadResponse>(&bytes).unwrap())
                .unwrap()
        };
        (status, value)
    }

    #[tokio::test]
    async fn checkpoint_routes_capture_trusted_baselines_and_survive_new_session() {
        let (directory, session, id) = recovery_session();
        let source_path = dunce::canonicalize(directory.path().join("src/Editor.luau")).unwrap();
        let body = serde_json::json!({ "sessionId": session.session_id(), "id": id, "source": "unsaved editor\n", "expectedSource": "old baseline\n", "messageCursor": 0, "sourcePath": "untrusted.luau" });
        let (status, response) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let checkpoint_id = response["checkpointId"].as_str().unwrap();
        let (status, listed) = recovery_call(
            session.clone(),
            Method::GET,
            "/api/source-checkpoints",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 1);
        assert!(listed["checkpoints"][0].get("source").is_none());
        assert_eq!(
            listed["checkpoints"][0]["sourcePath"],
            source_path.to_str().unwrap()
        );
        let route = format!("/api/source-checkpoints/{checkpoint_id}");
        drop(session);
        let restarted = Arc::new(
            ServeSession::new(
                memofs::Vfs::new_default().unwrap(),
                directory.path().join("default.project.json"),
            )
            .unwrap(),
        );
        let (status, read) = recovery_call(
            restarted.clone(),
            Method::GET,
            &route,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(read["checkpoint"]["source"], "unsaved editor\n");
        assert_eq!(read["checkpoint"]["expectedSource"], "old baseline\n");
        assert_eq!(read["checkpoint"]["canonicalSource"], "return 'disk'\n");
        assert_eq!(fs::read_to_string(source_path).unwrap(), "return 'disk'\n");
        assert_eq!(
            recovery_call(
                restarted.clone(),
                Method::DELETE,
                &route,
                serde_json::json!({"sessionId": restarted.session_id()})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            recovery_call(restarted, Method::GET, &route, serde_json::Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn checkpoint_capture_releases_tree_before_opening_durable_storage() {
        use crate::source_checkpoints::STORE_OPEN_OBSERVER;
        let (_directory, session, id) = recovery_session();
        session.vfs().set_watch_enabled(false);
        let tree = session.tree_handle();
        let observations = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = observations.clone();
        STORE_OPEN_OBSERVER.with_borrow_mut(|observer| {
            *observer = Some(Box::new(move || {
                assert!(
                    tree.try_lock().is_ok(),
                    "Durable storage opened while holding tree authority"
                );
                observed.set(observed.get() + 1);
            }));
        });
        let (status, response) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            serde_json::json!({"sessionId":session.session_id(), "id":id,
                "source":"draft", "expectedSource":"baseline", "messageCursor":0}),
        )
        .await;
        STORE_OPEN_OBSERVER.with_borrow_mut(|observer| *observer = None);
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(observations.get(), 1);
    }

    #[tokio::test]
    async fn checkpoint_routes_retain_project_identity_after_project_deletion_or_rename() {
        for rename in [false, true] {
            let (directory, session, id) = recovery_session();
            session.vfs().set_watch_enabled(false);
            let identity = session.checkpoint_identity().to_path_buf();
            let (status, created) = recovery_call(
                session.clone(),
                Method::POST,
                "/api/source-checkpoints",
                serde_json::json!({"sessionId":session.session_id(), "id":id,
                    "source":"retained draft", "expectedSource":"baseline", "messageCursor":0}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{created}");
            let route = format!(
                "/api/source-checkpoints/{}",
                created["checkpointId"].as_str().unwrap()
            );
            let project = directory.path().join("default.project.json");
            if rename {
                fs::rename(&project, directory.path().join("renamed.project.json")).unwrap();
            } else {
                fs::remove_file(&project).unwrap();
            }
            let (status, listed) = recovery_call(
                session.clone(),
                Method::GET,
                "/api/source-checkpoints",
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{listed}");
            assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 1);
            let (status, read) = recovery_call(
                session.clone(),
                Method::GET,
                &route,
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{read}");
            assert_eq!(read["checkpoint"]["source"], "retained draft");
            assert_eq!(
                read["checkpoint"]["provenance"]["projectIdentity"],
                serde_json::to_value(identity).unwrap()
            );
            let (status, discarded) = recovery_call(
                session.clone(),
                Method::DELETE,
                &route,
                serde_json::json!({"sessionId":session.session_id()}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{discarded}");
            assert_eq!(
                recovery_call(session, Method::GET, &route, serde_json::Value::Null)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn checkpoint_retired_descendant_keeps_trusted_baseline_without_recreating_files() {
        let (directory, session, id) = recovery_session();
        let source_path = directory.path().join("src/Editor.luau");
        fs::remove_file(&source_path).unwrap();
        {
            let mut tree = session.tree();
            let root = tree.get_root_id();
            let folder = tree.insert_instance(
                root,
                crate::snapshot::InstanceSnapshot::new().name("Removed"),
            );
            tree.reparent(id, folder).unwrap();
            crate::snapshot::apply_patch_set(
                &mut tree,
                PatchSet {
                    removed_instances: vec![folder],
                    ..PatchSet::new()
                },
            );
        }
        let body = serde_json::json!({"sessionId": session.session_id(), "id": id,
            "source": "local unfinished =", "expectedSource": "baseline", "messageCursor": 0});
        for (key, value) in [
            ("sessionId", serde_json::to_value(SessionId::new()).unwrap()),
            ("messageCursor", serde_json::json!(1)),
        ] {
            let mut invalid = body.clone();
            invalid[key] = value;
            assert_eq!(
                recovery_call(
                    session.clone(),
                    Method::POST,
                    "/api/source-checkpoints",
                    invalid
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert!(!directory.path().join(".rojo/source-checkpoints").exists());
        assert_retired_checkpoint(session, id, &source_path).await;
        assert!(!source_path.exists());
    }

    #[tokio::test]
    async fn checkpoint_retired_class_change_keeps_original_source_metadata() {
        let (directory, session, id) = recovery_session();
        let source_path = directory.path().join("src/Editor.luau");
        {
            let mut tree = session.tree();
            let mut metadata = tree.get_metadata(id).unwrap().clone();
            metadata.source_transform = Some(crate::snapshot::SourceTransform {
                original: "return require('@alias')".into(),
                emitted: "return 'disk'\n".into(),
            });
            tree.update_metadata(id, metadata);
            crate::snapshot::apply_patch_set(
                &mut tree,
                PatchSet {
                    updated_instances: vec![PatchUpdate {
                        id,
                        changed_name: None,
                        changed_parent: None,
                        changed_class_name: Some(ustr("Folder")),
                        changed_metadata: Some(crate::snapshot::InstanceMetadata::new()),
                        changed_properties: rbx_dom_weak::UstrMap::from_iter([(
                            ustr("Source"),
                            None,
                        )]),
                    }],
                    ..PatchSet::new()
                },
            );
        }
        let read = assert_retired_checkpoint(session, id, &source_path).await;
        assert_eq!(
            read["checkpoint"]["sourceTransform"]["original"],
            "return require('@alias')"
        );
        assert_eq!(
            read["checkpoint"]["sourceTransform"]["emitted"],
            "return 'disk'\n"
        );
        assert_eq!(fs::read_to_string(source_path).unwrap(), "return 'disk'\n");
    }

    async fn assert_retired_checkpoint(
        session: Arc<ServeSession>,
        id: Ref,
        path: &std::path::Path,
    ) -> serde_json::Value {
        let body = serde_json::json!({"sessionId": session.session_id(), "id": id,
            "source": "local unfinished =", "expectedSource": "old baseline", "messageCursor": 0,
            "sourcePath": "untrusted.luau"});
        let (status, created) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let route = format!(
            "/api/source-checkpoints/{}",
            created["checkpointId"].as_str().unwrap()
        );
        let (status, read) = recovery_call(
            session.clone(),
            Method::GET,
            &route,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(read["checkpoint"]["id"], serde_json::to_value(id).unwrap());
        assert_eq!(read["checkpoint"]["name"], "Editor");
        let canonical_path = dunce::canonicalize(path.parent().unwrap())
            .unwrap()
            .join(path.file_name().unwrap());
        assert_eq!(
            read["checkpoint"]["sourcePath"],
            canonical_path.to_str().unwrap()
        );
        assert_eq!(read["checkpoint"]["source"], "local unfinished =");
        assert_eq!(read["checkpoint"]["expectedSource"], "old baseline");
        assert_eq!(read["checkpoint"]["canonicalSource"], "return 'disk'\n");
        assert_eq!(session.message_queue().cursor(), 0);
        read
    }

    #[tokio::test]
    async fn checkpoint_retired_sources_preserve_writable_validation() {
        for rules in [
            serde_json::json!({"ignorePaths": ["src/**"]}),
            serde_json::json!({"ignoreTrees": ["Editor"]}),
            serde_json::json!({"ignoreProperties": {"LuaSourceContainer": ["Source"]}}),
        ] {
            let (directory, session, id) = recovery_session_with_rules(rules);
            assert_checkpoint_rejected(session.clone(), id).await;
            session.tree().remove(id);
            assert_checkpoint_rejected(session, id).await;
            assert_eq!(
                fs::read_to_string(directory.path().join("src/Editor.luau")).unwrap(),
                "return 'disk'\n"
            );
        }
        let (_directory, session, id) = recovery_session();
        let targets = {
            let mut tree = session.tree();
            let root = tree.get_root_id();
            let ids = tree
                .descendants(root)
                .filter(|instance| matches!(instance.name(), "Mapped" | "Part"))
                .map(|instance| instance.id())
                .collect::<Vec<_>>();
            for &target in &ids {
                tree.remove(target);
            }
            let mut metadata = tree.get_metadata(id).unwrap().clone();
            metadata.context.set_reserved_cache_path(
                metadata.relevant_paths[0].parent().unwrap().to_path_buf(),
            );
            tree.update_metadata(id, metadata);
            tree.remove(id);
            ids
        };
        for target in targets.into_iter().chain([id, Ref::new()]) {
            assert_checkpoint_rejected(session.clone(), target).await;
        }
    }

    #[tokio::test]
    async fn checkpoint_authorization_tracks_live_policy_for_live_and_retired_sources() {
        for rules in [
            serde_json::json!({"ignorePaths": ["src/**"]}),
            serde_json::json!({"ignoreTrees": ["Editor"]}),
            serde_json::json!({"ignoreProperties": {"LuaSourceContainer": ["Source"]}}),
        ] {
            for retired in [false, true] {
                for initially_excluded in [false, true] {
                    let initial = if initially_excluded {
                        rules.clone()
                    } else {
                        serde_json::json!({})
                    };
                    let (directory, session, id) = recovery_session_with_rules(initial);
                    if retired {
                        session
                            .vfs()
                            .remove_file(directory.path().join("src/Editor.luau"))
                            .unwrap();
                        session.tree().remove(id);
                    }
                    let project_path = session.root_project().file_location.clone();
                    let mut project: serde_json::Value =
                        serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
                    let excluded = !initially_excluded;
                    project["syncbackRules"] = if excluded {
                        rules.clone()
                    } else {
                        serde_json::json!({})
                    };
                    session
                        .vfs()
                        .write(&project_path, serde_json::to_vec(&project).unwrap())
                        .unwrap();
                    let expected =
                        crate::Project::load_exact(session.vfs(), &project_path, Some("Recovery"))
                            .unwrap()
                            .syncback_rules;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while session.current_project().syncback_rules != expected {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("The watcher must reload the changed policy");
                    let body = serde_json::json!({"sessionId": session.session_id(), "id": id,
                        "source": "preserve the draft", "expectedSource": "baseline", "messageCursor": 0});
                    let (status, response) = recovery_call(
                        session.clone(),
                        Method::POST,
                        "/api/source-checkpoints",
                        body,
                    )
                    .await;
                    assert_eq!(status, if excluded { StatusCode::BAD_REQUEST } else { StatusCode::OK },
                        "Retired {retired}, initially excluded {initially_excluded}, rules {rules}, response {response}");
                    if retired {
                        assert!(session.tree().get_instance(id).is_none());
                        assert!(!directory.path().join("src/Editor.luau").exists());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_retired_descendant_preserves_ancestor_ownership() {
        let (_directory, session, id) = recovery_session();
        let descendant = {
            let mut tree = session.tree();
            let root = tree.get_root_id();
            let owner = tree
                .descendants(root)
                .find(|instance| instance.name() == "Mapped")
                .unwrap()
                .id();
            let mut metadata = tree.get_metadata(id).unwrap().clone();
            metadata.instigating_source = None;
            let properties = tree.get_instance(id).unwrap().properties().clone();
            let descendant = tree.insert_instance(
                owner,
                crate::snapshot::InstanceSnapshot::new()
                    .name("Child")
                    .class_name("ModuleScript")
                    .metadata(metadata)
                    .properties(properties),
            );
            tree.remove(owner);
            descendant
        };
        assert_checkpoint_rejected(session, descendant).await;
    }

    #[tokio::test]
    async fn checkpoint_retired_sources_recheck_current_package_policy() {
        let (directory, session, id) = recovery_session();
        session.tree().remove(id);
        {
            let mut tree = session.tree();
            let root = tree.get_root_id();
            let mut metadata = tree.get_metadata(root).unwrap().clone();
            metadata.context.package_sources = Some(Arc::new(
                serde_json::from_value(serde_json::json!({
                    "root": directory.path(), "install": null, "problem": "Package writes blocked",
                    "local_sources": [], "deny_all_writes": true
                }))
                .unwrap(),
            ));
            tree.update_metadata(root, metadata);
        }
        assert_checkpoint_rejected(session, id).await;
    }

    async fn assert_checkpoint_rejected(session: Arc<ServeSession>, id: Ref) {
        let body = serde_json::json!({"sessionId": session.session_id(), "id": id,
            "source": "keep this draft", "expectedSource": "baseline", "messageCursor": 0,
            "sourcePath": "src/Editor.luau"});
        let (status, response) =
            recovery_call(session, Method::POST, "/api/source-checkpoints", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(response["details"]
            .as_str()
            .unwrap()
            .contains("Preserve editor Source"));
    }

    #[tokio::test]
    async fn checkpoint_retired_authority_never_overrides_a_live_script() {
        let (_directory, session, id) = recovery_session();
        {
            let mut tree = session.tree();
            tree.get_instance_mut(id)
                .unwrap()
                .set_class_name("LocalScript");
            assert!(tree.retired_source(id).is_some());
            let mut metadata = tree.get_metadata(id).unwrap().clone();
            metadata.context.set_reserved_cache_path(
                metadata.relevant_paths[0].parent().unwrap().to_path_buf(),
            );
            tree.update_metadata(id, metadata);
        }
        assert_checkpoint_rejected(session.clone(), id).await;
        session
            .tree()
            .get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .remove(&ustr("Source"));
        assert_checkpoint_rejected(session, id).await;
    }

    #[tokio::test]
    async fn checkpoint_retired_identity_is_not_remapped_to_replacement() {
        let (directory, session, id) = recovery_session();
        let replacement = {
            let mut tree = session.tree();
            let root = tree.get_root_id();
            let snapshot = crate::snapshot::InstanceSnapshot::new()
                .name("Editor")
                .class_name("ModuleScript")
                .metadata(tree.get_metadata(id).unwrap().clone())
                .properties(rbx_dom_weak::UstrMap::from_iter([(
                    ustr("Source"),
                    Variant::String("replacement".into()),
                )]));
            tree.remove(id);
            let replacement = tree.insert_instance(root, snapshot);
            assert_ne!(id, replacement);
            replacement
        };
        assert_retired_checkpoint(
            session.clone(),
            id,
            &directory.path().join("src/Editor.luau"),
        )
        .await;
        let body = serde_json::json!({"sessionId": session.session_id(), "id": replacement,
            "source": "replacement draft", "expectedSource": "replacement", "messageCursor": 0});
        let (status, created) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let (_, read) = recovery_call(
            session,
            Method::GET,
            &format!(
                "/api/source-checkpoints/{}",
                created["checkpointId"].as_str().unwrap()
            ),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(
            read["checkpoint"]["id"],
            serde_json::to_value(replacement).unwrap()
        );
        assert_eq!(read["checkpoint"]["canonicalSource"], "replacement");
    }

    #[tokio::test]
    async fn checkpoint_retired_reused_identity_does_not_inherit_authority() {
        let (_directory, session, id) = recovery_session();
        {
            let mut tree = session.tree();
            tree.remove(id);
            let root = tree.get_root_id();
            tree.insert_instance_with_referents(
                root,
                crate::snapshot::InstanceSnapshot::new()
                    .name("Editor")
                    .class_name("Folder"),
                &crate::snapshot::SnapshotReferents {
                    id: Some(id),
                    children: Vec::new(),
                },
                &mut Vec::new(),
            );
        }
        assert_checkpoint_rejected(session, id).await;
    }

    #[tokio::test]
    async fn checkpoint_retired_entry_payload_and_expiry_limits_fail_closed() {
        for limit in ["entries", "payload", "oversized", "expiry"] {
            let (directory, session, id) = recovery_session();
            let ids = {
                let mut tree = session.tree();
                let root = tree.get_root_id();
                let snapshot = crate::snapshot::InstanceSnapshot::new()
                    .name("Editor")
                    .class_name("ModuleScript")
                    .metadata(tree.get_metadata(id).unwrap().clone())
                    .properties(tree.get_instance(id).unwrap().properties().clone());
                let bytes = crate::source_checkpoints::SourceDescriptor::capture(&tree, id)
                    .unwrap()
                    .payload_bytes();
                tree.retired_source_limits(
                    if limit == "entries" { 2 } else { 128 },
                    match limit {
                        "payload" => bytes * 2,
                        "oversized" => bytes - 1,
                        _ => 16 * 1024 * 1024,
                    },
                    if limit == "expiry" {
                        std::time::Duration::ZERO
                    } else {
                        std::time::Duration::from_secs(600)
                    },
                );
                let second = tree.insert_instance(root, snapshot.clone());
                let third = tree.insert_instance(root, snapshot);
                for target in [id, second, third] {
                    tree.remove(target);
                }
                [id, second, third]
            };
            assert_checkpoint_rejected(session.clone(), ids[0]).await;
            if matches!(limit, "entries" | "payload") {
                for target in &ids[1..] {
                    assert_retired_checkpoint(
                        session.clone(),
                        *target,
                        &directory.path().join("src/Editor.luau"),
                    )
                    .await;
                }
            } else {
                for target in &ids[1..] {
                    assert_checkpoint_rejected(session.clone(), *target).await;
                }
            }
            assert_eq!(
                fs::read_to_string(directory.path().join("src/Editor.luau")).unwrap(),
                "return 'disk'\n"
            );
        }
    }

    #[tokio::test]
    async fn checkpoint_retired_eviction_preserves_durable_editor_records() {
        let (directory, session, id) = recovery_session();
        let second = {
            let mut tree = session.tree();
            tree.retired_source_limits(1, 16 * 1024 * 1024, std::time::Duration::from_secs(600));
            let root = tree.get_root_id();
            let snapshot = crate::snapshot::InstanceSnapshot::new()
                .name("Editor")
                .class_name("ModuleScript")
                .metadata(tree.get_metadata(id).unwrap().clone())
                .properties(tree.get_instance(id).unwrap().properties().clone());
            tree.insert_instance(root, snapshot)
        };
        session.tree().remove(id);
        let saved = assert_retired_checkpoint(
            session.clone(),
            id,
            &directory.path().join("src/Editor.luau"),
        )
        .await;
        session.tree().remove(second);
        assert_checkpoint_rejected(session.clone(), id).await;
        let (status, read) = recovery_call(
            session.clone(),
            Method::GET,
            &format!(
                "/api/source-checkpoints/{}",
                saved["checkpoint"]["checkpointId"].as_str().unwrap()
            ),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read, saved);
        let (_, listed) = recovery_call(
            session,
            Method::GET,
            "/api/source-checkpoints",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn checkpoint_retired_messagepack_post_does_not_authorize_write_route() {
        let (directory, session, id) = recovery_session();
        fs::remove_file(directory.path().join("src/Editor.luau")).unwrap();
        session.tree().remove(id);
        let value: crate::web::interface::SourceCheckpointRequest = serde_json::from_value(serde_json::json!({
            "sessionId": session.session_id(), "id": id, "source": "incomplete =", "expectedSource": "old", "messageCursor": 0
        })).unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/source-checkpoints")
            .body(Body::from(
                crate::web::util::serialize_msgpack(&value).unwrap(),
            ))
            .unwrap();
        let response = call(session.clone(), "127.0.0.1:1".parse().unwrap(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let write = serde_json::json!({"sessionId": session.session_id(), "messageCursor": 0,
            "authority": "Filesystem", "removed": [], "updated": [{"id": id, "changedProperties": {"Source": {"String": "must not write"}}}]});
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/write")
            .body(Body::from(
                crate::web::util::serialize_msgpack(&write).unwrap(),
            ))
            .unwrap();
        let response = call(session.clone(), "127.0.0.1:1".parse().unwrap(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = body::to_bytes(response.into_body()).await.unwrap();
        let response: WriteResponse = deserialize_msgpack(&bytes).unwrap();
        assert!(response.accepted.updated_instances.is_empty());
        assert!(!response.errors.is_empty());
        assert!(!directory.path().join("src/Editor.luau").exists());
        assert_eq!(session.message_queue().cursor(), 0);
    }

    #[tokio::test]
    async fn checkpoint_discard_requires_the_current_session_and_preserves_stale_requests() {
        let (_directory, session, id) = recovery_session();
        let body = serde_json::json!({ "sessionId": session.session_id(), "id": id, "source": "editor", "expectedSource": "baseline", "messageCursor": 0 });
        let (status, created) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let route = format!(
            "/api/source-checkpoints/{}",
            created["checkpointId"].as_str().unwrap()
        );
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!({"sessionId": SessionId::new()}),
        ] {
            let (status, response) =
                recovery_call(session.clone(), Method::DELETE, &route, invalid).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
            assert_eq!(
                recovery_call(
                    session.clone(),
                    Method::GET,
                    &route,
                    serde_json::Value::Null
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        assert_eq!(
            recovery_call(
                session.clone(),
                Method::DELETE,
                &route,
                serde_json::json!({"sessionId": session.session_id()})
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn checkpoint_routes_reject_invalid_sessions_ids_cursors_and_oversized_bodies() {
        let (_directory, session, id) = recovery_session();
        let body = serde_json::json!({ "sessionId": session.session_id(), "id": id, "source": "editor", "expectedSource": "baseline", "messageCursor": 0 });
        for (key, value) in [
            ("sessionId", serde_json::to_value(SessionId::new()).unwrap()),
            ("sessionId", serde_json::json!("malformed-session")),
            ("id", serde_json::to_value(Ref::new()).unwrap()),
            ("messageCursor", serde_json::json!(1)),
            ("id", serde_json::json!("malformed")),
        ] {
            let mut invalid = body.clone();
            invalid[key] = value;
            assert_eq!(
                recovery_call(
                    session.clone(),
                    Method::POST,
                    "/api/source-checkpoints",
                    invalid
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        for name in ["Part", "Mapped"] {
            let target = {
                let tree = session.tree();
                tree.descendants(tree.get_root_id())
                    .find(|instance| instance.name() == name)
                    .unwrap()
                    .id()
            };
            let mut invalid = body.clone();
            invalid["id"] = serde_json::to_value(target).unwrap();
            assert_eq!(
                recovery_call(
                    session.clone(),
                    Method::POST,
                    "/api/source-checkpoints",
                    invalid
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        for required in ["source", "expectedSource", "messageCursor"] {
            let mut invalid = body.clone();
            invalid.as_object_mut().unwrap().remove(required);
            assert_eq!(
                recovery_call(
                    session.clone(),
                    Method::POST,
                    "/api/source-checkpoints",
                    invalid
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        for id in [
            "owner.json",
            "../owner",
            "%2e%2e%2fowner",
            "00000000-0000-0000-0000-000000000000",
        ] {
            for method in [Method::GET, Method::DELETE] {
                assert_eq!(
                    recovery_call(
                        session.clone(),
                        method,
                        &format!("/api/source-checkpoints/{id}"),
                        serde_json::Value::Null
                    )
                    .await
                    .0,
                    StatusCode::BAD_REQUEST
                );
            }
        }
        let mut oversized = body;
        oversized["source"] = "x".repeat(crate::source_checkpoints::MAX_BODY_BYTES).into();
        assert_eq!(
            recovery_call(
                session.clone(),
                Method::POST,
                "/api/source-checkpoints",
                oversized
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let (_, listed) = recovery_call(
            session,
            Method::GET,
            "/api/source-checkpoints",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn checkpoint_storage_failure_is_actionable_and_never_changes_source() {
        let (directory, session, id) = recovery_session();
        fs::create_dir_all(directory.path().join(".rojo")).unwrap();
        fs::write(
            directory.path().join(".rojo/source-checkpoints"),
            "not a directory",
        )
        .unwrap();
        let body = serde_json::json!({ "sessionId": session.session_id(), "id": id, "source": "keep editor", "expectedSource": "baseline", "messageCursor": 0 });
        let (status, response) = recovery_call(
            session.clone(),
            Method::POST,
            "/api/source-checkpoints",
            body,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response["details"]
            .as_str()
            .unwrap()
            .contains("Preserve editor Source"));
        assert_eq!(session.message_queue().cursor(), 0);
        assert_eq!(
            fs::read_to_string(directory.path().join("src/Editor.luau")).unwrap(),
            "return 'disk'\n"
        );
    }

    #[tokio::test]
    async fn write_operation_is_echoed_and_published_before_acknowledgement() {
        let (_directory, session, id) = recovery_session();
        let request: WriteRequest = serde_json::from_value(serde_json::json!({ "sessionId": session.session_id(), "operationId": "write_1", "messageCursor": 0,
            "authority": "Filesystem", "removed": [], "updated": [{ "id": id, "changedProperties": { "Source": { "String": "return 'editor'\n" } } }] })).unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/write")
            .body(Body::from(serialize_msgpack(&request).unwrap()))
            .unwrap();
        let response = call(session.clone(), "127.0.0.1:1".parse().unwrap(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response: WriteResponse =
            deserialize_msgpack(&body::to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(response.operation_id.as_deref(), Some("write_1"));
        assert!(
            response.errors.is_empty(),
            "unexpected write errors {:?}",
            response.errors
        );
        let (cursor, published) = session
            .message_queue()
            .subscribe(0)
            .try_recv()
            .unwrap()
            .unwrap();
        assert_eq!(cursor, response.message_cursor);
        assert!(published
            .iter()
            .any(|patch| patch.source_operations.get(&id).map(String::as_str) == Some("write_1")));
        let capability = response
            .acknowledgement
            .expect("Accepted writes return private proof");
        assert!(session
            .message_queue()
            .valid_predecessor(&capability, Some(0)));
        assert!(!serde_json::to_string(&published)
            .unwrap()
            .contains(&capability));
        let request: WriteRequest = serde_json::from_value(serde_json::json!({
            "sessionId": session.session_id(), "operationId": "write_2", "messageCursor": 0,
            "predecessor": capability, "sourceBaselines": { id.to_string(): 0 },
            "authority": "Filesystem", "removed": [], "updated": [{ "id": id,
                "changedProperties": { "Source": { "String": "return 'editor'\n-- next\n" } } }]
        }))
        .unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/write")
            .body(Body::from(serialize_msgpack(&request).unwrap()))
            .unwrap();
        let response = call(session.clone(), "127.0.0.1:1".parse().unwrap(), request).await;
        let response: WriteResponse =
            deserialize_msgpack(&body::to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert_eq!(response.conflicts, PatchSet::new());
        assert!(response.acknowledgement.is_some());
    }

    #[tokio::test]
    async fn fresh_api_read_after_source_pressure_restores_small_writes_without_other_traffic() {
        async fn write(session: Arc<ServeSession>, value: serde_json::Value) -> WriteResponse {
            let request: WriteRequest = serde_json::from_value(value).unwrap();
            let request = Request::builder()
                .method(Method::POST)
                .uri("/api/write")
                .body(Body::from(serialize_msgpack(&request).unwrap()))
                .unwrap();
            let response = call(session, "127.0.0.1:1".parse().unwrap(), request).await;
            assert_eq!(response.status(), StatusCode::OK);
            deserialize_msgpack(&body::to_bytes(response.into_body()).await.unwrap()).unwrap()
        }

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("src");
        fs::create_dir(&source).unwrap();
        // Exercise actual admission above the bounded 4096-entry history budget.
        for index in 0..4100 {
            fs::write(source.join(format!("Editor{index}.luau")), "return 0\n").unwrap();
        }
        let project = directory.path().join("default.project.json");
        fs::write(
            &project,
            r#"{"name":"Pressure","tree":{"$className":"Folder","$path":"src"}}"#,
        )
        .unwrap();
        let session =
            Arc::new(ServeSession::new(memofs::Vfs::new_default().unwrap(), &project).unwrap());
        let scripts = {
            let tree = session.tree();
            tree.descendants(tree.get_root_id())
                .filter(|instance| instance.class_name() == ustr("ModuleScript"))
                .map(|instance| (instance.id(), instance.name().to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(scripts.len(), 4100);
        let baselines: serde_json::Map<String, serde_json::Value> = scripts
            .iter()
            .map(|(id, _)| (id.to_string(), serde_json::json!(0)))
            .collect();
        let updates = scripts
            .iter()
            .map(|(id, _)| {
                serde_json::json!({"id":id,"changedProperties":{"Source":{"String":"return 1\n"}}})
            })
            .collect::<Vec<_>>();
        let pressure = write(
            session.clone(),
            serde_json::json!({
                "sessionId":session.session_id(),"operationId":"source_pressure",
                "messageCursor":0,"sourceBaselines":baselines,"authority":"Filesystem",
                "removed":[],"updated":updates
            }),
        )
        .await;
        assert!(pressure.acknowledgement.is_none());
        assert!(pressure.accepted.updated_instances.is_empty());
        assert_eq!(pressure.errors.len(), scripts.len());
        assert!(pressure
            .errors
            .iter()
            .all(|error| error.message.contains("unknown or expired")));
        assert!(pressure.message_cursor > 0);

        for (id, name) in [scripts.first().unwrap(), scripts.last().unwrap()] {
            let stale = write(
                session.clone(),
                serde_json::json!({
                    "sessionId":session.session_id(),"operationId":"expired_draft",
                    "messageCursor":0,"sourceBaselines":{id.to_string():0},
                    "authority":"Filesystem","removed":[],"updated":[{
                        "id":id,"changedProperties":{"Source":{"String":"return 'stale'\n"}}
                    }]
                }),
            )
            .await;
            assert!(!stale.errors.is_empty());
            assert!(stale.acknowledgement.is_none());
            assert!(stale.accepted.updated_instances.is_empty());
            let cursor_before_read = session.message_queue().cursor();
            let request = Request::builder()
                .method(Method::GET)
                .uri(format!("/api/read/{id}"))
                .body(Body::empty())
                .unwrap();
            let response = call(session.clone(), "127.0.0.1:1".parse().unwrap(), request).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = body::to_bytes(response.into_body()).await.unwrap();
            let read: ReadResponse<'_> = deserialize_msgpack(&bytes).unwrap();
            assert_eq!(read.message_cursor, cursor_before_read);
            assert!(read.message_cursor >= pressure.message_cursor);
            assert_eq!(
                read.instances[id].properties[&ustr("Source")].as_ref(),
                &Variant::String("return 0\n".into())
            );
            let fresh = write(
                session.clone(),
                serde_json::json!({
                    "sessionId":read.session_id,"operationId":"fresh_after_pressure",
                    "messageCursor":read.message_cursor,"sourceBaselines":{id.to_string():read.message_cursor},
                    "authority":"Filesystem","removed":[],"updated":[{
                        "id":id,"changedProperties":{"Source":{"String":"return 'fresh'\n"}}
                    }]
                }),
            )
            .await;
            assert!(fresh.errors.is_empty(), "{:?}", fresh.errors);
            assert_eq!(fresh.conflicts, PatchSet::new());
            assert!(fresh.acknowledgement.is_some());
            assert_eq!(
                fs::read_to_string(source.join(format!("{name}.luau"))).unwrap(),
                "return 'fresh'\n"
            );
        }
    }

    #[tokio::test]
    async fn write_api_validation_errors_do_not_return_a_capability() {
        let (_directory, session, id) = recovery_session();
        let request: WriteRequest = serde_json::from_value(serde_json::json!({
            "sessionId": session.session_id(), "messageCursor": 0,
            "authority": "Filesystem", "removed": [], "updated": [{ "id": id,
                "changedMetadata": {"ignoreUnknownInstances": true},
                "changedProperties": { "Source": { "String": "return 'accepted portion'\n" } } }]
        }))
        .unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/write")
            .body(Body::from(serialize_msgpack(&request).unwrap()))
            .unwrap();
        let response = call(session, "127.0.0.1:1".parse().unwrap(), request).await;
        let response: WriteResponse =
            deserialize_msgpack(&body::to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert!(!response.errors.is_empty());
        assert!(response.acknowledgement.is_none());
    }

    #[test]
    fn write_diagnostics_report_counts_without_payloads() {
        let update = PatchUpdate {
            id: Ref::new(),
            changed_name: Some("PrivateScriptName".into()),
            changed_class_name: None,
            changed_parent: None,
            changed_properties: UstrMap::from_iter([
                (
                    ustr("Source"),
                    Some(Variant::String("private script text".into())),
                ),
                (ustr("PrivateProperty"), None),
            ]),
            changed_metadata: None,
        };
        let response = WriteResponse {
            acknowledgement: None,
            operation_id: None,
            session_id: SessionId::new(),
            message_cursor: 42,
            accepted: PatchSet {
                removed_instances: vec![Ref::new()],
                added_instances: Vec::new(),
                updated_instances: vec![update.clone()],
            },
            conflicts: PatchSet {
                removed_instances: vec![Ref::new(), Ref::new()],
                added_instances: Vec::new(),
                updated_instances: vec![update],
            },
            errors: vec![WriteError {
                kind: None,
                id: Ref::new(),
                property: Some("PrivateProperty".into()),
                message: "Private path and error details".into(),
            }],
        };
        let summary = write_response_summary(&response);
        assert_eq!(summary, write_response_summary(&response));
        assert_eq!(
            summary,
            "result cursor 42, accepted additions 0, removals 1, updates 1, properties 2, \
             conflict additions 0, removals 2, updates 1, properties 2, errors 1"
        );
        for payload in ["Private", "private script text", "Source", "Variant"] {
            assert!(!summary.contains(payload));
        }
    }
}
