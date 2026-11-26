//! Defines all the structs needed to interact with the Rojo Serve API. This is
//! useful for tests to be able to use the same data structures as the
//! implementation.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use rbx_dom_weak::{
    types::{Ref, Variant, VariantType},
    Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::{
    rojo_ref::without_link_attributes,
    session_id::SessionId,
    snapshot::{
        AppliedPatchSet, CapturedAdditionInstance, InstanceMetadata as RojoInstanceMetadata,
        InstanceWithMeta, PatchSet, RojoTree,
    },
};

pub use crate::serve_session::SyncDirection;
pub use crate::snapshot::PublicationOrigin;
pub use crate::syncback::from_studio::{ManifestEntry, StudioService};

/// Server version to report over the API, not exposed outside this crate.
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current protocol version, which is required to match. Version 6 added write
/// cursors to write requests and accepted, conflicted, and failed results to write
/// responses, which older peers neither send nor understand. Version 7 made write
/// requests carry the plugin's chosen `WriteAuthority`. Version 8 adds identity-preserving
/// instance reparenting through `changedParent` updates. Version 9 makes `added` carry
/// Studio-created subtrees as `InstanceAddition`s. Version 10 adds live Studio export jobs.
/// Version 11 adds capture-relative service reference targets to export manifests.
/// Version 12 requires publication cursors in write acknowledgements so delayed
/// announcements cannot overwrite newer Studio property edits.
pub const PROTOCOL_VERSION: u64 = 12;

/// Explicit-direction sessions require peers that enforce the immutable sync policy.
/// Keep ordinary sessions on version 12 so legacy sync and export remain compatible.
/// Version 13 makes older peers reject directional sessions before reading the tree.
pub const DIRECTIONAL_PROTOCOL_VERSION: u64 = 13;

/// Message returned by Rojo API when a change has occurred.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeMessage<'a> {
    /// Trusted publication provenance, including payload-free cursor advances.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PublicationOrigin>,
    pub removed: Vec<Ref>,
    pub added: HashMap<Ref, Instance<'a>>,
    pub updated: Vec<InstanceUpdate>,
    /// Causal tokens apply only to the Source field of these updates.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub source_operations: HashMap<Ref, String>,
}

impl<'a> SubscribeMessage<'a> {
    pub(crate) fn from_patch_update(tree: &'a RojoTree, patch: AppliedPatchSet) -> Self {
        let removed = patch.removed;

        let mut added = HashMap::new();
        if patch.origin == Some(PublicationOrigin::Studio) {
            // A missing capture is not permission to attribute current disk state
            // to Studio. Fail closed without consulting the mutable tree.
            if let Some(captured) = &patch.captured_additions {
                for instance in captured.values() {
                    added.insert(instance.id, Instance::from_captured_instance(instance));
                }
            } else if !patch.added.is_empty() {
                log::error!("Studio addition publication has no captured state.");
            }
        } else {
            for id in patch.added {
                // Filesystem messages are read from the tree when sent, and an
                // instance added then removed again is already gone. A later
                // message removes it, so there's nothing to announce.
                let Some(instance) = tree.get_instance(id) else {
                    continue;
                };
                added.insert(id, Instance::from_rojo_instance(instance));

                for instance in tree.descendants(id) {
                    added.insert(instance.id(), Instance::from_rojo_instance(instance));
                }
            }
        }

        let updated = patch
            .updated
            .into_iter()
            .map(|update| {
                let changed_metadata = update
                    .changed_metadata
                    .as_ref()
                    .map(InstanceMetadata::from_rojo_metadata);

                let changed_properties = update
                    .changed_properties
                    .into_iter()
                    .map(|(key, value)| {
                        let value = value.or_else(|| property_default(tree, update.id, key));
                        let value = value.map(|value| without_link_attributes(&value).into_owned());
                        (key, value)
                    })
                    .filter(|(_key, value)| property_filter(value.as_ref()))
                    .collect();

                InstanceUpdate {
                    id: update.id,
                    changed_name: update.changed_name,
                    changed_class_name: update.changed_class_name,
                    changed_parent: update.changed_parent,
                    changed_properties,
                    changed_metadata,
                }
            })
            .collect();

        Self {
            origin: patch.origin,
            removed,
            added,
            updated,
            source_operations: patch.source_operations,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceUpdate {
    pub id: Ref,
    pub changed_name: Option<String>,
    pub changed_class_name: Option<Ustr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_parent: Option<Ref>,

    // TODO: Transform from UstrMap<String, Option<_>> to something else, since
    // null will get lost when decoding from JSON in some languages.
    #[serde(default, deserialize_with = "deserialize_changed_properties")]
    pub changed_properties: UstrMap<Option<Variant>>,
    pub changed_metadata: Option<InstanceMetadata>,
}

/// Reads property maps and attributes, accepting empty Luau tables sent as arrays.
///
/// The plugin's msgpack encoder writes every empty Luau table as an array, including
/// empty service property maps and the map inside an `Attributes` variant. Nonempty
/// arrays are never accepted in place of maps.
pub(crate) fn deserialize_changed_properties<'de, D>(
    deserializer: D,
) -> Result<UstrMap<Option<Variant>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, MapAccess, SeqAccess, Visitor};

    struct PropertiesVisitor;

    impl<'de> Visitor<'de> for PropertiesVisitor {
        type Value = UstrMap<Option<Variant>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map of property names to values")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(A::Error::custom(
                    "Expected a property map or an empty array.",
                ));
            }
            Ok(UstrMap::default())
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut properties = UstrMap::default();
            while let Some(name) = map.next_key::<Ustr>()? {
                let value = if name == "Attributes" {
                    map.next_value::<Option<LenientAttributesVariant>>()?
                        .map(|lenient| lenient.0)
                } else {
                    map.next_value::<Option<Variant>>().map_err(|error| {
                        A::Error::custom(format!("Invalid property \"{name}\". {error}"))
                    })?
                };
                properties.insert(name, value);
            }
            Ok(properties)
        }
    }

    deserializer.deserialize_any(PropertiesVisitor)
}

/// An `Attributes` variant, `{"Attributes": <map>}`, whose map may also be sent
/// as an empty array.
struct LenientAttributesVariant(Variant);

impl<'de> Deserialize<'de> for LenientAttributesVariant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};

        struct TaggedVisitor;

        impl<'de> Visitor<'de> for TaggedVisitor {
            type Value = LenientAttributesVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an Attributes value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let tag: String = map
                    .next_key()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                if tag != "Attributes" {
                    return Err(A::Error::custom(format!(
                        "expected the Attributes property to hold Attributes, not {tag}"
                    )));
                }
                let attributes = map.next_value::<AttributesOrEmpty>()?.0;
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom("expected a single Attributes value"));
                }
                Ok(LenientAttributesVariant(Variant::Attributes(attributes)))
            }
        }

        deserializer.deserialize_map(TaggedVisitor)
    }
}

/// An attribute map, or an empty array standing in for an empty map.
struct AttributesOrEmpty(rbx_dom_weak::types::Attributes);

impl<'de> Deserialize<'de> for AttributesOrEmpty {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{value::MapAccessDeserializer, Error, MapAccess, SeqAccess, Visitor};

        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = AttributesOrEmpty;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map of attributes, or an empty array")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::invalid_type(
                        serde::de::Unexpected::Seq,
                        &"a map of attributes, or an empty array",
                    ));
                }
                Ok(AttributesOrEmpty(Default::default()))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                Deserialize::deserialize(MapAccessDeserializer::new(map)).map(AttributesOrEmpty)
            }
        }

        deserializer.deserialize_any(EntriesVisitor)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMetadata {
    pub ignore_unknown_instances: bool,
}

impl InstanceMetadata {
    pub(crate) fn from_rojo_metadata(meta: &RojoInstanceMetadata) -> Self {
        Self {
            ignore_unknown_instances: meta.ignore_unknown_instances,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Instance<'a> {
    pub id: Ref,
    pub parent: Ref,
    pub name: Cow<'a, str>,
    pub class_name: Ustr,
    pub properties: UstrMap<Cow<'a, Variant>>,
    pub children: Cow<'a, [Ref]>,
    pub metadata: Option<InstanceMetadata>,
}

impl Instance<'_> {
    fn from_captured_instance(source: &CapturedAdditionInstance) -> Instance<'static> {
        let properties = source
            .properties
            .iter()
            .filter(|(_key, value)| property_filter(Some(value)))
            .map(|(key, value)| {
                (
                    *key,
                    Cow::Owned(without_link_attributes(value).into_owned()),
                )
            })
            .collect();

        Instance {
            id: source.id,
            parent: source.parent,
            name: Cow::Owned(source.name.clone()),
            class_name: source.class_name,
            properties,
            children: Cow::Owned(source.children.clone()),
            metadata: Some(InstanceMetadata::from_rojo_metadata(&source.metadata)),
        }
    }

    pub(crate) fn from_rojo_instance(source: InstanceWithMeta<'_>) -> Instance<'_> {
        let properties = source
            .properties()
            .iter()
            .filter(|(_key, value)| property_filter(Some(value)))
            .map(|(key, value)| (*key, without_link_attributes(value)))
            .collect();

        Instance {
            id: source.id(),
            parent: source.parent(),
            name: Cow::Borrowed(source.name()),
            class_name: source.class_name(),
            properties,
            children: Cow::Borrowed(source.children()),
            metadata: Some(InstanceMetadata::from_rojo_metadata(source.metadata())),
        }
    }
}

/// MessagePack nil map entries disappear in Luau. Materialize wire defaults without
/// changing the normalized property omissions in the DOM or on disk.
fn property_default(tree: &RojoTree, id: Ref, key: Ustr) -> Option<Variant> {
    let instance = tree.get_instance(id)?;
    let class_name = instance.class_name();
    // References have no reflection default, and an explicit null reference is the
    // only way to clear one.
    if is_ref_property(class_name.as_str(), key.as_str()) {
        return Some(Variant::Ref(Ref::none()));
    }
    let database = rbx_reflection_database::get().unwrap();
    database
        .classes
        .get(class_name.as_str())?
        .default_properties
        .get(key.as_str())
        .cloned()
}

/// Freeze only omitted accepted properties at worker completion, before later tree changes.
/// Existing response values and all non-property fields retain their original meaning.
pub(crate) fn materialize_accepted_properties(tree: &RojoTree, accepted: &mut PatchSet) {
    for update in &mut accepted.updated_instances {
        for (key, value) in &mut update.changed_properties {
            if value.is_none() {
                *value = property_default(tree, update.id, *key);
            }
        }
    }
}

/// Returns whether `class_name.prop_name` holds a reference, checking the
/// classes it inherits from.
fn is_ref_property(class_name: &str, prop_name: &str) -> bool {
    let database = rbx_reflection_database::get().unwrap();
    let mut current = class_name;
    while let Some(class) = database.classes.get(current) {
        if let Some(property) = class.properties.get(prop_name) {
            return matches!(
                property.data_type,
                rbx_reflection::DataType::Value(VariantType::Ref)
            );
        }
        match class.superclass {
            Some(superclass) => current = superclass,
            None => break,
        }
    }
    false
}

fn property_filter(value: Option<&Variant>) -> bool {
    let ty = value.map(|value| value.ty());

    // Lua can't do anything with SharedString values. They also can't be
    // serialized directly by Serde!
    ty != Some(VariantType::SharedString)
}

/// Response body from /api/rojo
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_direction: Option<SyncDirection>,
    /// Plain strings with a default, so clients can read older servers and ignore
    /// directions added later.
    #[serde(default)]
    pub supported_sync_directions: Vec<String>,
    pub session_id: SessionId,
    pub server_version: String,
    pub protocol_version: u64,
    pub project_name: String,
    pub expected_place_ids: Option<HashSet<u64>>,
    pub unexpected_place_ids: Option<HashSet<u64>>,
    pub game_id: Option<u64>,
    pub place_id: Option<u64>,
    pub root_instance_id: Ref,
}

/// Response body from /api/read/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResponse<'a> {
    pub session_id: SessionId,
    pub message_cursor: u32,
    pub instances: HashMap<Ref, Instance<'a>>,
}

/// Which side wins when a two-way sync write meets a change on disk that
/// Studio hasn't seen yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteAuthority {
    /// Structural and file-payload conflicts restore the disk value in Studio.
    /// Ordinary properties merge Studio's values when the target's structural
    /// baseline is still valid.
    Filesystem,
    /// The server catches up with the disk, then writes every value Studio
    /// changed over it. Disk changes Studio didn't touch are kept.
    Studio,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
    pub session_id: SessionId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_operation_id"
    )]
    pub operation_id: Option<String>,
    /// A private capability returned by the preceding accepted write in this chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<String>,
    /// Per-source last applied publication cursors at draft capture time. These do
    /// not advance when a packet or acknowledgement merely includes newer Source.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub source_baselines: HashMap<Ref, u32>,
    /// The last server revision applied before this Studio batch began.
    #[serde(default)]
    pub message_cursor: Option<u32>,
    pub authority: WriteAuthority,
    pub removed: Vec<Ref>,

    /// Subtrees created in Studio, keyed by the id Studio gave each root.
    #[serde(default)]
    pub added: HashMap<Ref, InstanceAddition>,
    pub updated: Vec<InstanceUpdate>,
}

fn deserialize_operation_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let token = Option::<String>::deserialize(deserializer)?;
    if token.as_ref().is_some_and(|value| {
        value.is_empty()
            || value.len() > 96
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }) {
        return Err(serde::de::Error::custom("operationId must contain 1 to 96 ASCII letters, digits, underscores, hyphens, or periods"));
    }
    Ok(token)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCheckpointRequest {
    pub session_id: SessionId,
    pub id: Ref,
    pub source: String,
    pub expected_source: String,
    pub message_cursor: u32,
}

/// A subtree created in Studio. Studio picks every id itself and starts
/// tracking the instances under them before sending, so the server's
/// announcement of the same instances is recognized instead of duplicated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceAddition {
    /// The managed instance the subtree was created under.
    pub parent: Ref,
    /// The subtree serialized by Studio as a binary model with one root.
    #[serde(with = "serde_bytes")]
    pub model: Vec<u8>,
    /// The ids Studio assigned, shaped like the subtree.
    pub ids: AddedIds,
}

/// The id Studio assigned to one instance of an added subtree. A model doesn't
/// keep Studio's ids, so the name and class let the server pair each id with
/// its instance. Same-named siblings pair in their order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedIds {
    pub id: Ref,
    pub name: String,
    pub class_name: Ustr,
    #[serde(default)]
    pub children: Vec<AddedIds>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResponse {
    pub session_id: SessionId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_operation_id"
    )]
    pub operation_id: Option<String>,
    /// Opaque session-local proof of this successful write's exact effects and
    /// its validated predecessors. Never included in subscription publications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement: Option<String>,
    /// Every announcement or restoration produced by this write is published
    /// at or before this cursor. This is not causal coverage or Source application.
    pub message_cursor: u32,
    pub accepted: PatchSet,
    pub conflicts: PatchSet,
    pub errors: Vec<WriteError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriteErrorKind {
    SourceSerializationDeferred,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<WriteErrorKind>,
    pub id: Ref,
    pub property: Option<String>,
    pub message: String,
}

pub type WriteResult = (PatchSet, PatchSet, Vec<WriteError>);

/// Packet type enum for different websocket message types
#[derive(Debug, Serialize, Deserialize, Display, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum SocketPacketType {
    Export,
    Messages,
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize,
    // RefPatch,
}

/// Body content for messages packet type
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPacket<'a> {
    pub message_cursor: u32,
    pub messages: Vec<SubscribeMessage<'a>>,
}

/// Body content for different packet types
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SocketPacketBody<'a> {
    Export(super::export::CaptureRequest),
    Messages(MessagesPacket<'a>),
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize(SerializePacket),
    // RefPatch(RefPatchPacket<'a>),
}

/// Message content from /api/socket
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketPacket<'a> {
    pub session_id: SessionId,
    pub packet_type: SocketPacketType,
    pub body: SocketPacketBody<'a>,
}

/// Response body from /api/open/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResponse {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeRequest {
    pub session_id: SessionId,
    pub ids: Vec<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeResponse {
    pub session_id: SessionId,
    #[serde(with = "serde_bytes")]
    pub model_contents: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchRequest {
    pub session_id: SessionId,
    pub ids: HashSet<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchResponse<'a> {
    pub session_id: SessionId,
    pub patch: SubscribeMessage<'a>,
}

/// Response to `GET /api/syncback` from a `rojo syncback --from-studio` server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncbackInfoResponse {
    pub server_version: String,
    pub protocol_version: u64,
    pub syncback_id: SessionId,
    pub project_name: String,
    pub services: Vec<StudioService>,

    /// The largest part, in bytes, the plugin should upload in one request.
    pub part_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncbackStartRequest {
    pub syncback_id: SessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncbackStartResponse {
    /// Issued by the server for each attempt, so requests left over from a
    /// failed attempt never mix with a retry.
    pub upload_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncbackFinishRequest {
    pub syncback_id: SessionId,
    pub upload_id: u64,
    pub part_count: u32,
    pub manifest: Vec<ManifestEntry>,

    /// Where `Workspace.CurrentCamera` is, if it was sent. The first element is a
    /// zero-based manifest index, and the rest are zero-based child indices below
    /// that instance.
    #[serde(default)]
    pub current_camera: Option<Vec<usize>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncbackFinishResponse {
    pub syncback_id: SessionId,
    pub instance_count: usize,
    pub dropped: Vec<ManifestEntry>,
}

/// General response type returned from all Rojo routes
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    kind: ErrorResponseKind,
    details: String,
}

impl ErrorResponse {
    pub fn not_found<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::NotFound,
            details: details.into(),
        }
    }

    pub fn bad_request<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::BadRequest,
            details: details.into(),
        }
    }

    pub fn forbidden<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::Forbidden,
            details: details.into(),
        }
    }

    pub fn internal_error<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::InternalError,
            details: details.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ErrorResponseKind {
    NotFound,
    BadRequest,
    Forbidden,
    InternalError,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{apply_patch_set, InstanceSnapshot, PatchUpdate};
    use rbx_dom_weak::ustr;

    #[test]
    fn legacy_ack_shared_defaults_preserve_publication_filtering_and_missing_instance_semantics() {
        use rbx_dom_weak::types::{Attributes, SharedString};

        let tree = RojoTree::new(InstanceSnapshot::new().class_name("Script"));
        let id = tree.get_root_id();
        let missing = Ref::new();
        let message = SubscribeMessage::from_patch_update(
            &tree,
            AppliedPatchSet {
                updated: vec![
                    crate::snapshot::AppliedPatchUpdate {
                        id,
                        changed_name: None,
                        changed_class_name: None,
                        changed_parent: None,
                        changed_metadata: None,
                        changed_properties: UstrMap::from_iter([
                            (ustr("RunContext"), None),
                            (ustr("Source"), Some(Variant::String("retained".into()))),
                            (ustr("UnknownProperty"), None),
                            (
                                ustr("Shared"),
                                Some(Variant::SharedString(SharedString::new(vec![1]))),
                            ),
                            (
                                ustr("Attributes"),
                                Some(Variant::Attributes(Attributes::from_iter([
                                    ("Rojo_Id".into(), Variant::String("private-link".into())),
                                    ("Visible".into(), Variant::Bool(true)),
                                ]))),
                            ),
                        ]),
                    },
                    crate::snapshot::AppliedPatchUpdate {
                        id: missing,
                        changed_name: None,
                        changed_class_name: None,
                        changed_parent: None,
                        changed_metadata: None,
                        changed_properties: UstrMap::from_iter([(ustr("RunContext"), None)]),
                    },
                ],
                ..AppliedPatchSet::new()
            },
        );
        let properties = &message.updated[0].changed_properties;
        assert_eq!(
            properties[&ustr("RunContext")],
            Some(Variant::Enum(rbx_dom_weak::types::Enum::from_u32(0)))
        );
        assert_eq!(
            properties[&ustr("Source")],
            Some(Variant::String("retained".into()))
        );
        assert_eq!(properties[&ustr("UnknownProperty")], None);
        assert!(!properties.contains_key(&ustr("Shared")));
        assert_eq!(
            properties[&ustr("Attributes")],
            Some(Variant::Attributes(Attributes::from_iter([(
                "Visible".into(),
                Variant::Bool(true)
            ),])))
        );
        assert_eq!(
            message.updated[1].changed_properties[&ustr("RunContext")],
            None
        );
        assert!(tree.root().properties().is_empty());
    }

    #[test]
    fn subscription_provenance_survives_wire_conversion_and_both_encodings() {
        let tree = RojoTree::new(InstanceSnapshot::new());
        for origin in [PublicationOrigin::Filesystem, PublicationOrigin::Studio] {
            let message = SubscribeMessage::from_patch_update(
                &tree,
                AppliedPatchSet {
                    origin: Some(origin),
                    ..AppliedPatchSet::new()
                },
            );
            assert_eq!(message.origin, Some(origin));
            let json = serde_json::to_value(&message).unwrap();
            assert_eq!(json["origin"], serde_json::to_value(origin).unwrap());
            let encoded = rmp_serde::to_vec_named(&message).unwrap();
            let decoded: SubscribeMessage<'_> = rmp_serde::from_slice(&encoded).unwrap();
            assert_eq!(decoded.origin, Some(origin));
            assert!(
                decoded.removed.is_empty()
                    && decoded.added.is_empty()
                    && decoded.updated.is_empty()
            );
        }
    }

    #[test]
    fn studio_additions_without_capture_never_read_the_live_tree() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("LaterDiskState"));
        let message = SubscribeMessage::from_patch_update(
            &tree,
            AppliedPatchSet {
                origin: Some(PublicationOrigin::Studio),
                added: vec![tree.get_root_id()],
                ..AppliedPatchSet::new()
            },
        );
        assert!(message.added.is_empty());
    }

    #[test]
    fn operation_id_round_trips_and_rejects_unsafe_tokens() {
        let body = serde_json::json!({
            "sessionId": SessionId::new(), "operationId": "editor_1.write-2",
            "authority": "Filesystem", "removed": [], "updated": []
        });
        let request: WriteRequest = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap()["operationId"],
            body["operationId"]
        );
        let decoded: WriteRequest =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&request).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["operationId"],
            body["operationId"]
        );
        for token in [
            "".to_owned(),
            "a".repeat(97),
            "not safe".into(),
            "../escape".into(),
            "é".into(),
        ] {
            let mut invalid = body.clone();
            invalid["operationId"] = token.into();
            assert!(serde_json::from_value::<WriteRequest>(invalid).is_err());
        }
    }

    #[test]
    fn causal_contract_is_optional_and_keeps_source_cursors_separate() {
        let id = Ref::new();
        let mut body = serde_json::json!({
            "sessionId": SessionId::new(), "authority":"Filesystem",
            "messageCursor": 80, "removed": [], "updated": []
        });
        let old: WriteRequest = serde_json::from_value(body.clone()).unwrap();
        assert!(old.predecessor.is_none());
        assert!(old.source_baselines.is_empty());
        assert!(serde_json::to_value(old)
            .unwrap()
            .get("predecessor")
            .is_none());
        body["predecessor"] = "private-server-capability".into();
        body["sourceBaselines"] = serde_json::json!({ id.to_string(): 72 });
        let request: WriteRequest = serde_json::from_value(body).unwrap();
        let roundtrip: WriteRequest =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&request).unwrap()).unwrap();
        assert_eq!(roundtrip.message_cursor, Some(80));
        assert_eq!(roundtrip.source_baselines[&id], 72);
        assert_eq!(
            roundtrip.predecessor.as_deref(),
            Some("private-server-capability")
        );
        let response: WriteResponse = serde_json::from_value(serde_json::json!({
            "sessionId": request.session_id, "messageCursor": 81,
            "accepted": PatchSet::new(), "conflicts": PatchSet::new(), "errors": []
        }))
        .unwrap();
        assert!(response.acknowledgement.is_none());
    }

    /// An instance created and removed again before its message is sent is
    /// left out, instead of the server panicking while it holds the tree.
    #[test]
    fn messages_leave_out_additions_that_are_gone() {
        let tree = RojoTree::new(InstanceSnapshot::new().name("Root"));
        let gone = Ref::new();
        let message = SubscribeMessage::from_patch_update(
            &tree,
            AppliedPatchSet {
                added: vec![gone],
                removed: vec![gone],
                ..AppliedPatchSet::new()
            },
        );
        assert!(message.added.is_empty());
        assert_eq!(message.removed, vec![gone]);
    }

    /// Encodes an update body the way the plugin does, with msgpack from a
    /// loosely shaped value.
    fn plugin_update(changed_properties: serde_json::Value) -> Vec<u8> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            id: Ref,
            changed_name: Option<String>,
            changed_class_name: Option<String>,
            changed_properties: serde_json::Value,
            changed_metadata: Option<()>,
        }
        rmp_serde::to_vec_named(&Body {
            id: Ref::new(),
            changed_name: None,
            changed_class_name: None,
            changed_properties,
            changed_metadata: None,
        })
        .unwrap()
    }

    // Removing an instance's last attribute is sent as an empty array, which
    // used to fail the whole write.
    #[test]
    fn reads_an_empty_attributes_array_as_no_attributes() {
        let bytes = plugin_update(serde_json::json!({ "Attributes": { "Attributes": [] } }));
        let decoded: InstanceUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            decoded.changed_properties.get(&ustr("Attributes")),
            Some(&Some(Variant::Attributes(Default::default())))
        );
    }

    #[test]
    fn reads_attributes_and_other_properties_unchanged() {
        let mut attributes = rbx_dom_weak::types::Attributes::new();
        attributes.insert("Health".into(), Variant::Float64(5.0));
        attributes.insert("Label".into(), Variant::String("a".into()));
        let mut changed_properties = UstrMap::default();
        changed_properties.insert(ustr("Attributes"), Some(Variant::Attributes(attributes)));
        changed_properties.insert(ustr("Name"), Some(Variant::String("Renamed".into())));
        changed_properties.insert(
            ustr("Data"),
            Some(Variant::BinaryString(vec![0u8, 1, 255].into())),
        );
        changed_properties.insert(ustr("Reset"), None);
        let update = InstanceUpdate {
            id: Ref::new(),
            changed_parent: None,
            changed_name: None,
            changed_class_name: None,
            changed_properties,
            changed_metadata: None,
        };

        let from_msgpack: InstanceUpdate =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&update).unwrap()).unwrap();
        let from_json: InstanceUpdate =
            serde_json::from_value(serde_json::to_value(&update).unwrap()).unwrap();
        for decoded in [from_msgpack, from_json] {
            assert_eq!(decoded.changed_properties, update.changed_properties);
        }
    }

    #[test]
    fn rejects_attributes_that_are_not_an_attribute_map() {
        for value in [
            serde_json::json!({ "Attributes": { "Attributes": [1] } }),
            serde_json::json!({ "Attributes": { "String": "x" } }),
            serde_json::json!({ "Attributes": { "Attributes": {}, "String": "x" } }),
        ] {
            let bytes = plugin_update(value.clone());
            assert!(
                rmp_serde::from_slice::<InstanceUpdate>(&bytes).is_err(),
                "accepted {}",
                value
            );
        }
    }

    #[test]
    fn acknowledgement_protocol_requires_cursor_capable_peers() {
        assert_eq!(PROTOCOL_VERSION, 12);
        let config = include_str!("../../plugin/src/Config.lua");
        assert!(config.contains("protocolVersion = 12,"));
    }

    #[test]
    fn write_acknowledgement_cursor_is_required_in_json_and_messagepack() {
        let response = WriteResponse {
            acknowledgement: Some("server-issued-private-proof".into()),
            operation_id: Some("roundtrip_1".into()),
            session_id: SessionId::new(),
            message_cursor: 7,
            accepted: PatchSet::new(),
            conflicts: PatchSet::new(),
            errors: Vec::new(),
        };
        let mut json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["messageCursor"], 7);
        let from_json: WriteResponse = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(from_json.message_cursor, 7);
        assert_eq!(from_json.operation_id, response.operation_id);
        let from_msgpack: WriteResponse =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&response).unwrap()).unwrap();
        assert_eq!(from_msgpack.message_cursor, 7);
        assert_eq!(from_msgpack.operation_id, response.operation_id);
        json.as_object_mut().unwrap().remove("messageCursor");
        let error = serde_json::from_value::<WriteResponse>(json).unwrap_err();
        assert!(error.to_string().contains("missing field `messageCursor`"));

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct CursorlessResponse {
            session_id: SessionId,
            accepted: PatchSet,
            conflicts: PatchSet,
            errors: Vec<WriteError>,
        }
        let cursorless = CursorlessResponse {
            session_id: response.session_id,
            accepted: response.accepted,
            conflicts: response.conflicts,
            errors: response.errors,
        };
        let error =
            rmp_serde::from_slice::<WriteResponse>(&rmp_serde::to_vec_named(&cursorless).unwrap())
                .unwrap_err();
        assert!(error.to_string().contains("missing field `messageCursor`"));
    }

    #[test]
    fn structural_protocol_round_trips_json_and_messagepack() {
        assert_eq!(PROTOCOL_VERSION, 12);
        let id = Ref::new();
        let parent = Ref::new();
        let update = InstanceUpdate {
            id,
            changed_parent: Some(parent),
            changed_name: None,
            changed_class_name: None,
            changed_properties: Default::default(),
            changed_metadata: None,
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["changedParent"], serde_json::to_value(parent).unwrap());
        assert!(json.get("changed_parent").is_none());
        let decoded: InstanceUpdate = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.changed_parent, Some(parent));
        let bytes = rmp_serde::to_vec_named(&update).unwrap();
        let decoded: InstanceUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.changed_parent, Some(parent));

        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let child = tree.insert_instance(root, InstanceSnapshot::new());
        let destination = tree.insert_instance(root, InstanceSnapshot::new());
        let patch = PatchSet {
            updated_instances: vec![PatchUpdate {
                id: child,
                changed_parent: Some(destination),
                changed_name: None,
                changed_class_name: None,
                changed_properties: Default::default(),
                changed_metadata: None,
            }],
            ..Default::default()
        };
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(
            json["updated_instances"][0]["changedParent"],
            serde_json::to_value(destination).unwrap()
        );
        let patch: PatchSet =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&patch).unwrap()).unwrap();
        let applied = apply_patch_set(&mut tree, patch);
        let applied_json = serde_json::to_value(&applied).unwrap();
        assert_eq!(
            applied_json["updated"][0]["changedParent"],
            serde_json::to_value(destination).unwrap()
        );
        let message = SubscribeMessage::from_patch_update(&tree, applied);
        let message: SubscribeMessage =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&message).unwrap()).unwrap();
        assert_eq!(message.updated[0].changed_parent, Some(destination));
        assert!(message.added.is_empty());
        assert!(message.removed.is_empty());
    }

    #[test]
    fn absent_parent_is_a_compatible_non_structural_update() {
        let value = serde_json::json!({ "id": Ref::new() });
        let update: InstanceUpdate = serde_json::from_value(value).unwrap();
        assert_eq!(update.changed_parent, None);
        assert!(serde_json::to_value(update)
            .unwrap()
            .get("changedParent")
            .is_none());
        let patch = PatchUpdate {
            id: Ref::new(),
            changed_parent: None,
            changed_name: None,
            changed_class_name: None,
            changed_properties: Default::default(),
            changed_metadata: None,
        };
        let json = serde_json::to_value(patch).unwrap();
        assert!(json.get("changedParent").is_none());
        assert_eq!(
            serde_json::from_value::<PatchUpdate>(json)
                .unwrap()
                .changed_parent,
            None
        );
    }

    #[test]
    fn removed_references_are_sent_as_explicit_null_refs() {
        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("ObjectValue"));
        let id = tree.get_root_id();
        let applied = apply_patch_set(
            &mut tree,
            PatchSet {
                updated_instances: vec![PatchUpdate {
                    id,
                    changed_name: None,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_properties: UstrMap::from_iter([(ustr("Value"), None)]),
                    changed_metadata: None,
                }],
                ..PatchSet::new()
            },
        );
        let message = SubscribeMessage::from_patch_update(&tree, applied);
        assert_eq!(
            message.updated[0].changed_properties[&ustr("Value")],
            Some(Variant::Ref(Ref::none()))
        );
        // Inherited reference properties count too, like Model.PrimaryPart.
        assert!(is_ref_property("Model", "PrimaryPart"));
        assert!(is_ref_property("ObjectValue", "Value"));
        assert!(!is_ref_property("Part", "Transparency"));
        assert!(!is_ref_property("NotARealClass", "Value"));
    }

    #[test]
    fn absent_properties_have_explicit_wire_defaults_without_mutating_the_tree() {
        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("Part"));
        let id = tree.get_root_id();
        let applied = apply_patch_set(
            &mut tree,
            PatchSet {
                updated_instances: vec![PatchUpdate {
                    id,
                    changed_name: None,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_properties: UstrMap::from_iter([(ustr("Transparency"), None)]),
                    changed_metadata: None,
                }],
                ..PatchSet::new()
            },
        );
        let message = SubscribeMessage::from_patch_update(&tree, applied);
        assert_eq!(
            message.updated[0].changed_properties[&ustr("Transparency")],
            Some(Variant::Float32(0.0))
        );
        assert!(!tree
            .get_instance(id)
            .unwrap()
            .properties()
            .contains_key(&ustr("Transparency")));
    }
}
