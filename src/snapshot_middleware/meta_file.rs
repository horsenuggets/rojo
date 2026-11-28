use std::path::{Path, PathBuf};

use anyhow::{format_err, Context};
use indexmap::IndexMap;
use memofs::{IoResultExt as _, Vfs};
use rbx_dom_weak::{
    types::{Attributes, Variant},
    Ustr,
};
use serde::{Deserialize, Serialize};

use crate::{
    json,
    resolution::{
        normalize_json_variant, normalize_property_names, AmbiguousValue, UnresolvedValue,
    },
    snapshot::{InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackSnapshot},
    variant_eq::variant_eq,
    RojoRef, REF_ID_ATTRIBUTE_NAME,
};

/// Stages metadata in its existing format, including an empty rewrite that
/// clears old overrides. Both files are applied by the readers, so dual files
/// cannot safely receive the same payload (IDs, schemas, and inline children
/// would be applied twice). Like model and scoped writers, refuse that case
/// rather than removing a file or changing ignored/scoped ownership.
pub(super) fn write_metadata<T: Serialize>(
    vfs: &Vfs,
    output: &mut FsSnapshot,
    json_path: &Path,
    metadata: &T,
    empty: bool,
) -> anyhow::Result<()> {
    let jsonc_path = json_path.with_extension("jsonc");
    let is_file = |path: &Path| -> anyhow::Result<bool> {
        Ok(vfs
            .metadata(path)
            .with_not_found()?
            .is_some_and(|metadata| metadata.is_file()))
    };
    let json_exists = is_file(json_path)?;
    let jsonc_exists = is_file(&jsonc_path)?;
    anyhow::ensure!(
        !(json_exists && jsonc_exists),
        "Both {} and {} hold this instance's metadata, so it can't be safely rewritten",
        json_path.display(),
        jsonc_path.display()
    );
    if !empty || json_exists || jsonc_exists {
        let path = if jsonc_exists { &jsonc_path } else { json_path };
        output.add_file(
            path,
            json::to_vec_pretty(metadata).context("could not serialize metadata")?,
        );
    }
    Ok(())
}

/// Splits serializable values into metadata properties and attributes, sorted
/// by name.
///
/// Instance properties live in a hash map whose order changes from run to run,
/// so sorting is what makes a fresh syncback of the same place write the same
/// files every time.
///
/// Internal `RBX` attributes are left out.
pub fn serialize_values<'a>(
    class: &str,
    values: impl IntoIterator<Item = (Ustr, &'a Variant)>,
) -> (
    IndexMap<Ustr, UnresolvedValue>,
    IndexMap<String, UnresolvedValue>,
) {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    let mut properties = IndexMap::new();
    let mut attributes = IndexMap::new();
    for (name, value) in values {
        match value {
            Variant::Attributes(attrs) => {
                for (attr_name, attr_value) in attrs.iter() {
                    if attr_name.starts_with("RBX") {
                        continue;
                    }
                    attributes.insert(
                        attr_name.clone(),
                        UnresolvedValue::from_variant_unambiguous(attr_value.clone()),
                    );
                }
            }
            _ => {
                properties.insert(
                    name,
                    UnresolvedValue::from_variant(value.clone(), class, &name),
                );
            }
        }
    }
    (properties, attributes)
}

/// Returns the `id` to write to a metadata file, given the ID the instance
/// already has and the attributes being written.
///
/// Reading a file lets its `Rojo_Id` attribute override the metadata ID. Keeping
/// an old ID beside a new attribute would write a stale value that disappears
/// on the next syncback. Only keep the metadata ID when no valid attribute
/// supplies the instance's ID.
pub(super) fn metadata_id(
    specified_id: Option<&RojoRef>,
    attributes: &IndexMap<String, UnresolvedValue>,
) -> Option<String> {
    let id = specified_id?.to_string();
    let carried = match attributes.get(REF_ID_ATTRIBUTE_NAME) {
        Some(UnresolvedValue::FullyQualified(Variant::String(_)))
        | Some(UnresolvedValue::Ambiguous(AmbiguousValue::String(_))) => true,
        Some(UnresolvedValue::FullyQualified(Variant::BinaryString(value))) => {
            str::from_utf8(value.as_ref()).is_ok()
        }
        _ => false,
    };
    (!carried).then_some(id)
}

/// The key order of a metadata file that already exists. Rewriting the file in
/// this order keeps unchanged keys where they were, so an edit to one property
/// shows up as a one-line diff instead of the whole file being reshuffled.
#[derive(Debug, Default, PartialEq)]
struct ExistingKeyOrder {
    properties: Vec<String>,
    attributes: Vec<String>,
}

/// The values a metadata file already holds, as written.
#[derive(Debug, Default, Deserialize)]
struct ExistingValues {
    #[serde(default)]
    properties: IndexMap<Ustr, UnresolvedValue>,
    #[serde(default)]
    attributes: IndexMap<String, UnresolvedValue>,
}

/// A metadata file that already exists, used to rewrite it with the smallest
/// possible diff: keys keep their order, and values that didn't change keep
/// exactly the text they had.
#[derive(Debug, Default)]
struct ExistingMetadata {
    order: ExistingKeyOrder,
    values: ExistingValues,
}

impl ExistingMetadata {
    /// Reads `path` or its `.jsonc` sibling. A missing or unreadable file
    /// leaves everything as it was produced.
    fn read(vfs: &Vfs, path: &Path) -> Self {
        let jsonc = path.with_extension("jsonc");
        for candidate in [path, jsonc.as_path()] {
            if let Ok(Some(contents)) = vfs.read(candidate).with_not_found() {
                let Ok(text) = str::from_utf8(&contents) else {
                    continue;
                };
                if let Some(order) = ExistingKeyOrder::parse(text) {
                    return Self {
                        order,
                        values: json::from_str(text).unwrap_or_default(),
                    };
                }
            }
        }
        Self::default()
    }

    fn apply(
        &self,
        class: &str,
        properties: &mut IndexMap<Ustr, UnresolvedValue>,
        attributes: &mut IndexMap<String, UnresolvedValue>,
    ) {
        for (key, value) in properties.iter_mut() {
            if let Some(old) = self.values.properties.get(key) {
                if same_value(
                    old.clone().resolve(class, key),
                    value.clone().resolve(class, key),
                ) {
                    *value = old.clone();
                }
            }
        }
        for (key, value) in attributes.iter_mut() {
            if let Some(old) = self.values.attributes.get(key) {
                if same_value(
                    old.clone().resolve_unambiguous(),
                    value.clone().resolve_unambiguous(),
                ) {
                    *value = old.clone();
                }
            }
        }
        self.order.apply(properties, attributes);
    }
}

fn same_value(old: anyhow::Result<Variant>, new: anyhow::Result<Variant>) -> bool {
    let (Ok(old), Ok(new)) = (old, new) else {
        return false;
    };
    // Approximate equality preserves decimal spellings, but must not restore values
    // whose original datatype still needs numeric normalization.
    variant_eq(&old, &new)
        && serde_json::to_value(&old).expect("Variants must serialize")
            == serde_json::to_value(normalize_json_variant(old)).expect("Variants must serialize")
}

impl ExistingKeyOrder {
    /// Reads key order from the syntax tree, since parsing into a JSON value
    /// sorts object keys.
    fn parse(text: &str) -> Option<Self> {
        use jsonc_parser::ast::Value;

        let parsed =
            jsonc_parser::parse_to_ast(text, &Default::default(), &Default::default()).ok()?;
        let Some(Value::Object(root)) = parsed.value else {
            return None;
        };
        let keys = |value: &Value| match value {
            Value::Object(object) => object
                .properties
                .iter()
                .map(|property| property.name.clone().into_string())
                .collect(),
            _ => Vec::new(),
        };
        let mut order = Self::default();
        for property in &root.properties {
            match property.name.clone().into_string().as_str() {
                "properties" => order.properties = keys(&property.value),
                "attributes" => order.attributes = keys(&property.value),
                _ => {}
            }
        }
        Some(order)
    }

    /// Sorts existing keys into their old positions and leaves new keys after
    /// them in the order they were produced.
    fn apply(
        &self,
        properties: &mut IndexMap<Ustr, UnresolvedValue>,
        attributes: &mut IndexMap<String, UnresolvedValue>,
    ) {
        let position = |keys: &[String], key: &str| {
            keys.iter()
                .position(|existing| existing == key)
                .unwrap_or(usize::MAX)
        };
        properties.sort_by_key(|key, _| position(&self.properties, key.as_str()));
        attributes.sort_by_key(|key, _| position(&self.attributes, key.as_str()));
    }
}

/// A descendant stored inside its ancestor's `init.meta.json` instead of in
/// its own files. The fields mirror the children of a `.model.json` file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineInstance {
    pub class_name: Ustr,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub attributes: IndexMap<String, UnresolvedValue>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<InlineInstance>,
}

impl InlineInstance {
    fn into_snapshot(self, path: &Path) -> anyhow::Result<InstanceSnapshot> {
        let class_name = self.class_name;
        let name = self.name.unwrap_or_else(|| class_name.to_string());

        let mut properties = rbx_dom_weak::UstrMap::default();
        for (key, unresolved) in normalize_property_names(&class_name, self.properties) {
            let value = unresolved
                .resolve(&class_name, &key)
                .with_context(|| format!("error applying meta file {}", path.display()))?;
            properties.insert(key, value);
        }
        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();
            for (key, unresolved) in self.attributes {
                attributes.insert(key, unresolved.resolve_unambiguous()?);
            }
            properties.insert("Attributes".into(), attributes.into());
        }

        let children = self
            .children
            .into_iter()
            .map(|child| child.into_snapshot(path))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(InstanceSnapshot::new()
            .name(name)
            .class_name(class_name)
            .properties(properties)
            .children(children)
            .metadata(InstanceMetadata::new().specified_id(self.id.map(RojoRef::new))))
    }
}

/// Represents metadata in a sibling file with the same basename.
///
/// As an example, hello.meta.json next to hello.lua would allow assigning
/// additional metadata to the instance resulting from hello.lua.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjacentMetadata {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub(super) schema: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_unknown_instances: Option<bool>,

    /// The Instance's real name, for when its file name can't hold it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub attributes: IndexMap<String, UnresolvedValue>,

    #[serde(skip)]
    pub path: PathBuf,
}

/// Renames the snapshot to the name stored in metadata, if there is one.
fn apply_name(name: &mut Option<String>, snapshot: &mut InstanceSnapshot) {
    if let Some(name) = name.take() {
        snapshot.name = name.into();
    }
}

impl AdjacentMetadata {
    /// Attempts to read a meta file for the provided path and name, and if
    /// one exists applies it.
    ///
    /// Also inserts the potential metadata paths into the snapshot's relevant
    /// paths for convenience purposes.
    pub fn read_and_apply_all(
        vfs: &Vfs,
        path: &Path,
        name: &str,
        snapshot: &mut InstanceSnapshot,
    ) -> anyhow::Result<()> {
        let meta_path_json = path.with_file_name(format!("{name}.meta.json"));
        let meta_path_jsonc = path.with_file_name(format!("{name}.meta.jsonc"));

        if let Some(meta_contents) = vfs.read(&meta_path_json).with_not_found()? {
            let mut metadata = Self::from_slice(&meta_contents, meta_path_json.clone())?;
            metadata.apply_all(snapshot)?;
        }

        if let Some(meta_contents) = vfs.read(&meta_path_jsonc).with_not_found()? {
            let mut metadata = Self::from_slice(&meta_contents, meta_path_json.clone())?;
            metadata.apply_all(snapshot)?;
        }

        // Rather than pushing these in the snapshot middleware, we can just do it here.
        snapshot.metadata.relevant_paths.push(meta_path_json);
        snapshot.metadata.relevant_paths.push(meta_path_jsonc);

        Ok(())
    }

    fn from_slice(slice: &[u8], path: PathBuf) -> anyhow::Result<Self> {
        let mut meta: Self = json::from_slice_with_context(slice, || {
            format!(
                "File contained malformed .meta.json data: {}",
                path.display()
            )
        })?;

        meta.path = path;
        Ok(meta)
    }

    /// Constructs an `AdjacentMetadata` from the provided snapshot, assuming it
    /// will be at the provided path.
    pub fn from_syncback_snapshot(
        snapshot: &SyncbackSnapshot,
        path: PathBuf,
    ) -> anyhow::Result<Option<Self>> {
        // TODO make this more granular.
        // I am breaking the cycle of bad TODOs. This is in reference to the fact
        // that right now, this will just not write any metadata at all for
        // project nodes, which is not always desirable. We should try to be
        // smarter about it.
        if let Some(old_inst) = snapshot.old_inst() {
            if let Some(source) = &old_inst.metadata().instigating_source {
                let source = source.path();
                if source != path && !snapshot.is_script_suffix_migration(&path) {
                    log::debug!(
                        "Instigating source for Instance is mismatched so its metadata is being skipped.\nPath: {}",
                        path.display()
                    );
                    return Ok(None);
                }
            }
        }

        let ignore_unknown_instances = snapshot
            .old_metadata()
            .map(|metadata| metadata.ignore_unknown_instances)
            .unwrap_or_default();

        let schema = snapshot
            .old_metadata()
            .and_then(|metadata| metadata.schema.clone());

        let class = &snapshot.new_inst().class;
        let (mut properties, mut attributes) = serialize_values(
            class,
            snapshot.get_path_filtered_properties(snapshot.new).unwrap(),
        );
        if let Some(parent) = path.parent() {
            let meta_path = parent.join(format!("{}.meta.json", snapshot.file_stem()));
            ExistingMetadata::read(snapshot.vfs(), &meta_path).apply(
                class,
                &mut properties,
                &mut attributes,
            );
        }

        let id = metadata_id(
            snapshot
                .old_metadata()
                .and_then(|metadata| metadata.specified_id.as_ref()),
            &attributes,
        );

        Ok(Some(Self {
            ignore_unknown_instances: if ignore_unknown_instances {
                Some(true)
            } else {
                None
            },
            name: snapshot.name_override(),
            properties,
            attributes,
            path,
            id,
            schema,
        }))
    }

    pub fn apply_ignore_unknown_instances(&mut self, snapshot: &mut InstanceSnapshot) {
        if let Some(ignore) = self.ignore_unknown_instances.take() {
            snapshot.metadata.ignore_unknown_instances = ignore;
        }
    }

    pub fn apply_properties(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        let path = &self.path;

        for (key, unresolved) in
            normalize_property_names(&snapshot.class_name, std::mem::take(&mut self.properties))
        {
            let value = unresolved
                .resolve(&snapshot.class_name, &key)
                .with_context(|| format!("error applying meta file {}", path.display()))?;

            snapshot.properties.insert(key, value);
        }

        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();

            for (key, unresolved) in std::mem::take(&mut self.attributes) {
                let value = unresolved.resolve_unambiguous()?;
                attributes.insert(key, value);
            }

            snapshot
                .properties
                .insert("Attributes".into(), attributes.into());
        }

        Ok(())
    }

    fn apply_id(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        if self.id.is_some() && snapshot.metadata.specified_id.is_some() {
            anyhow::bail!(
                "cannot specify an ID using {} (instance has an ID from somewhere else)",
                self.path.display()
            );
        }
        if let Some(id) = self.id.take() {
            snapshot.metadata.specified_id = Some(RojoRef::new(id));
        }
        Ok(())
    }

    fn apply_schema(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        if self.schema.is_some() && snapshot.metadata.schema.is_some() {
            anyhow::bail!("cannot specify a schema using {} (instance has a schema from somewhere else. how did we get here?)", self.path.display());
        }
        if let Some(schema) = self.schema.take() {
            snapshot.metadata.schema = Some(schema);
        }
        Ok(())
    }

    pub fn apply_all(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        self.apply_ignore_unknown_instances(snapshot);
        apply_name(&mut self.name, snapshot);
        self.apply_properties(snapshot)?;
        self.apply_id(snapshot)?;
        self.apply_schema(snapshot)?;
        Ok(())
    }

    /// Returns whether the metadata is 'empty', meaning it doesn't have anything
    /// worth persisting in it. Specifically:
    ///
    /// - The number of properties and attributes is 0
    /// - `ignore_unknown_instances`, `id`, and `schema` are None
    /// - `name` is None
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
            && self.properties.is_empty()
            && self.ignore_unknown_instances.is_none()
            && self.id.is_none()
            && self.schema.is_none()
            && self.name.is_none()
    }

    /// Returns what the metadata file at `path` should hold next to a model
    /// file whose root is named `name`. The model file stores the root's
    /// properties and attributes itself, so any that `existing` sets are
    /// dropped, or they would override the synced values when read back. Root
    /// identity and ignore behavior come from `metadata` even for fresh layouts;
    /// valid IDs in the model's attributes remain authoritative. No other source
    /// metadata is inherited. Returns `None` when nothing would be left to
    /// store, so the file shouldn't exist.
    pub fn for_model_root(
        existing: Option<&[u8]>,
        path: &Path,
        name: Option<String>,
        metadata: Option<&InstanceMetadata>,
        attributes: &IndexMap<String, UnresolvedValue>,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let mut meta = match existing {
            Some(contents) => Self::from_slice(contents, path.to_path_buf())?,
            None => Self {
                schema: None,
                id: None,
                ignore_unknown_instances: None,
                name: None,
                properties: IndexMap::new(),
                attributes: IndexMap::new(),
                path: path.to_path_buf(),
            },
        };
        if let Some(metadata) = metadata {
            meta.id = metadata.specified_id.as_ref().map(ToString::to_string);
            meta.schema = metadata.schema.clone();
            // A fresh destination has no sidecar spelling to retain. False is the
            // reader's default, but an existing explicit false remains explicit.
            meta.ignore_unknown_instances = if metadata.ignore_unknown_instances {
                Some(true)
            } else {
                meta.ignore_unknown_instances.map(|_| false)
            };
        }
        let specified_id = meta.id.take().map(RojoRef::new);
        meta.id = metadata_id(specified_id.as_ref(), attributes);
        meta.name = name;
        meta.properties.clear();
        meta.attributes.clear();
        if meta.is_empty() {
            return Ok(None);
        }
        let contents = json::to_vec_pretty(&meta).context("could not serialize metadata")?;
        Ok(Some(contents))
    }

    // TODO: Add method to allow selectively applying parts of metadata and
    // throwing errors if invalid parts are specified.
}

/// Represents metadata that affects the instance resulting from the containing
/// folder.
///
/// This is always sourced from a file named init.meta.json.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_name: Option<Ustr>,

    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_unknown_instances: Option<bool>,

    /// The Instance's real name, for when its directory name can't hold it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub attributes: IndexMap<String, UnresolvedValue>,

    /// Descendants written into this file instead of their own files. They
    /// follow any children that come from files in the directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<InlineInstance>,

    #[serde(skip)]
    pub path: PathBuf,
}

impl DirectoryMetadata {
    /// Attempts to read an `init.meta`` file for the provided path, and if
    /// one exists applies it.
    ///
    /// Also inserts the potential metadata paths into the snapshot's relevant
    /// paths for convenience purposes.
    pub fn read_and_apply_all(
        vfs: &Vfs,
        path: &Path,
        snapshot: &mut InstanceSnapshot,
    ) -> anyhow::Result<()> {
        let meta_path_json = path.join("init.meta.json");
        let meta_path_jsonc = path.join("init.meta.jsonc");

        if let Some(meta_contents) = vfs.read(&meta_path_json).with_not_found()? {
            let mut metadata = Self::from_slice(&meta_contents, meta_path_json.clone())?;
            metadata.apply_all(snapshot)?;
        }

        if let Some(meta_contents) = vfs.read(&meta_path_jsonc).with_not_found()? {
            let mut metadata = Self::from_slice(&meta_contents, meta_path_jsonc.clone())?;
            metadata.apply_all(snapshot)?;
        }

        // Rather than pushing these in the snapshot middleware, we can just do it here.
        snapshot.metadata.relevant_paths.push(meta_path_json);
        snapshot.metadata.relevant_paths.push(meta_path_jsonc);

        Ok(())
    }

    fn from_slice(slice: &[u8], path: PathBuf) -> anyhow::Result<Self> {
        let mut meta: Self = json::from_slice_with_context(slice, || {
            format!(
                "File contained malformed init.meta.json data: {}",
                path.display()
            )
        })?;

        meta.path = path;
        Ok(meta)
    }

    /// Constructs a `DirectoryMetadata` from the provided snapshot, assuming it
    /// will be at the provided path.
    ///
    /// This function does not set `ClassName` manually as most uses won't
    /// want it set.
    pub fn from_syncback_snapshot(
        snapshot: &SyncbackSnapshot,
        path: PathBuf,
    ) -> anyhow::Result<Option<Self>> {
        // TODO make this more granular.
        // I am breaking the cycle of bad TODOs. This is in reference to the fact
        // that right now, this will just not write any metadata at all for
        // project nodes, which is not always desirable. We should try to be
        // smarter about it.
        if let Some(old_inst) = snapshot.old_inst() {
            if let Some(source) = &old_inst.metadata().instigating_source {
                let source = source.path();
                if source != path {
                    log::debug!(
                        "Instigating source for Instance is mismatched so its metadata is being skipped.\nPath: {}",
                        path.display()
                    );
                    return Ok(None);
                }
            }
        }

        let ignore_unknown_instances = snapshot
            .old_metadata()
            .map(|metadata| metadata.ignore_unknown_instances)
            .unwrap_or_default();

        let schema = snapshot
            .old_metadata()
            .and_then(|metadata| metadata.schema.clone());

        let class = &snapshot.new_inst().class;
        let (mut properties, mut attributes) = serialize_values(
            class,
            snapshot.get_path_filtered_properties(snapshot.new).unwrap(),
        );
        ExistingMetadata::read(snapshot.vfs(), &path.join("init.meta.json")).apply(
            class,
            &mut properties,
            &mut attributes,
        );

        let id = metadata_id(
            snapshot
                .old_metadata()
                .and_then(|metadata| metadata.specified_id.as_ref()),
            &attributes,
        );

        Ok(Some(Self {
            ignore_unknown_instances: if ignore_unknown_instances {
                Some(true)
            } else {
                None
            },
            name: snapshot.name_override(),
            properties,
            attributes,
            children: Vec::new(),
            class_name: None,
            path,
            id,
            schema,
        }))
    }

    pub fn apply_all(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        self.apply_ignore_unknown_instances(snapshot);
        apply_name(&mut self.name, snapshot);
        self.apply_class_name(snapshot)?;
        self.apply_properties(snapshot)?;
        self.apply_id(snapshot)?;
        self.apply_schema(snapshot)?;
        self.apply_children(snapshot)?;

        Ok(())
    }

    fn apply_children(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        for child in std::mem::take(&mut self.children) {
            snapshot.children.push(child.into_snapshot(&self.path)?);
        }
        Ok(())
    }

    fn apply_class_name(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        if let Some(class_name) = self.class_name.take() {
            if snapshot.class_name != "Folder" {
                // TODO: Turn into error type
                return Err(format_err!(
                    "className in init.meta.json can only be specified if the \
                     affected directory would turn into a Folder instance."
                ));
            }

            snapshot.class_name = class_name;
        }

        Ok(())
    }

    fn apply_ignore_unknown_instances(&mut self, snapshot: &mut InstanceSnapshot) {
        if let Some(ignore) = self.ignore_unknown_instances.take() {
            snapshot.metadata.ignore_unknown_instances = ignore;
        }
    }

    fn apply_properties(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        let path = &self.path;

        for (key, unresolved) in
            normalize_property_names(&snapshot.class_name, std::mem::take(&mut self.properties))
        {
            let value = unresolved
                .resolve(&snapshot.class_name, &key)
                .with_context(|| format!("error applying meta file {}", path.display()))?;

            snapshot.properties.insert(key, value);
        }

        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();

            for (key, unresolved) in std::mem::take(&mut self.attributes) {
                let value = unresolved.resolve_unambiguous()?;
                attributes.insert(key, value);
            }

            snapshot
                .properties
                .insert("Attributes".into(), attributes.into());
        }

        Ok(())
    }

    fn apply_id(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        if self.id.is_some() && snapshot.metadata.specified_id.is_some() {
            anyhow::bail!(
                "cannot specify an ID using {} (instance has an ID from somewhere else)",
                self.path.display()
            );
        }
        snapshot.metadata.specified_id = self.id.take().map(RojoRef::new);
        Ok(())
    }

    fn apply_schema(&mut self, snapshot: &mut InstanceSnapshot) -> anyhow::Result<()> {
        if self.schema.is_some() && snapshot.metadata.schema.is_some() {
            anyhow::bail!("cannot specify a schema using {} (instance has a schema from somewhere else. how did we get here?)", self.path.display());
        }
        snapshot.metadata.schema = self.schema.take();
        Ok(())
    }
    /// Returns whether the metadata is 'empty', meaning it doesn't have anything
    /// worth persisting in it. Specifically:
    ///
    /// - The number of properties and attributes is 0
    /// - `ignore_unknown_instances`, `id`, and `schema` are None
    /// - `name` is None
    /// - `class_name` is either None or Some("Folder")
    /// - There are no inline children
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
            && self.properties.is_empty()
            && self.children.is_empty()
            && self.ignore_unknown_instances.is_none()
            && self.id.is_none()
            && self.schema.is_none()
            && self.name.is_none()
            && if let Some(class) = &self.class_name {
                class == "Folder"
            } else {
                true
            }
    }
}

/// Retrieves the meta file that should be applied for the provided directory,
/// if it exists.
pub fn dir_meta(vfs: &Vfs, path: &Path) -> anyhow::Result<Option<DirectoryMetadata>> {
    let meta_path = path.join("init.meta.json");

    if let Some(meta_contents) = vfs.read(&meta_path).with_not_found()? {
        let metadata = DirectoryMetadata::from_slice(&meta_contents, meta_path)?;
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

/// Retrieves the meta file that should be applied for the provided file,
/// if it exists.
///
/// The `name` field should be the name the metadata should have.
pub fn file_meta(vfs: &Vfs, path: &Path, name: &str) -> anyhow::Result<Option<AdjacentMetadata>> {
    let mut meta_path = path.with_file_name(name);
    meta_path.set_extension("meta.json");

    if let Some(meta_contents) = vfs.read(&meta_path).with_not_found()? {
        let metadata = AdjacentMetadata::from_slice(&meta_contents, meta_path)?;
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod test {
    use memofs::{InMemoryFs, VfsSnapshot};
    use rbx_dom_weak::ustr;

    use super::*;

    #[test]
    fn root_metadata_emptiness_includes_identity_and_schema() {
        for id in [false, true] {
            for schema in [false, true] {
                for ignore in [None, Some(false), Some(true)] {
                    let mut value = serde_json::json!({});
                    if id {
                        value["id"] = "owner".into();
                    }
                    if schema {
                        value["$schema"] = "schema.json".into();
                    }
                    if let Some(ignore) = ignore {
                        value["ignoreUnknownInstances"] = ignore.into();
                    }
                    let adjacent: AdjacentMetadata = serde_json::from_value(value.clone()).unwrap();
                    let directory: DirectoryMetadata =
                        serde_json::from_value(value.clone()).unwrap();
                    let empty = !id && !schema && ignore.is_none();
                    assert_eq!(adjacent.is_empty(), empty, "adjacent {value}");
                    assert_eq!(directory.is_empty(), empty, "directory {value}");
                }
            }
        }
    }

    #[test]
    fn model_to_empty_folder_relayout_keeps_root_only_metadata() {
        use crate::{serve_session::ServeSession, syncback::syncback_loop};
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for extension in ["json", "jsonc"] {
            for bits in 0..8 {
                for ignore in [false, true] {
                    let temp = tempfile::tempdir().unwrap();
                    let vfs = Vfs::new_default().unwrap();
                    vfs.set_watch_enabled(false);
                    let project_path = temp.path().join("default.project.json");
                    vfs.write(&project_path, r#"{"name":"Test","tree":{"$path":"src"},"syncbackRules":{"instancesAsDirectories":true}}"#).unwrap();
                    vfs.create_dir_all(temp.path().join("src")).unwrap();
                    let old = WeakDom::new(
                        InstanceBuilder::new("DataModel")
                            .with_child(InstanceBuilder::new("Folder").with_name("Root")),
                    );
                    let mut model = Vec::new();
                    rbx_binary::to_writer(&mut model, &old, old.root().children()).unwrap();
                    vfs.write(temp.path().join("src/Root.rbxm"), model).unwrap();
                    let mut meta = serde_json::json!({});
                    if bits & 1 != 0 {
                        meta["id"] = "owner".into();
                    }
                    if bits & 2 != 0 {
                        meta["$schema"] = "schema.json".into();
                    }
                    if bits & 4 != 0 {
                        meta["ignoreUnknownInstances"] = ignore.into();
                    }
                    let old_sidecar = temp.path().join(format!("src/Root.meta.{extension}"));
                    vfs.write(&old_sidecar, serde_json::to_vec(&meta).unwrap())
                        .unwrap();
                    let session = ServeSession::new(vfs, &project_path).unwrap();
                    let incoming = WeakDom::new(
                        InstanceBuilder::new("Folder")
                            .with_child(InstanceBuilder::new("Folder").with_name("Root")),
                    );
                    let output = syncback_loop(
                        session.vfs(),
                        &mut session.tree(),
                        incoming,
                        session.root_project(),
                    )
                    .unwrap();
                    output.write_to_vfs(temp.path(), session.vfs()).unwrap();
                    assert!(!session.vfs().exists(&old_sidecar).unwrap());
                    let rebuilt = crate::snapshot_middleware::snapshot_from_vfs(
                        &crate::snapshot::InstanceContext::default(),
                        session.vfs(),
                        &temp.path().join("src/Root"),
                    )
                    .unwrap()
                    .unwrap();
                    assert_eq!(
                        rebuilt
                            .metadata
                            .specified_id
                            .as_ref()
                            .map(ToString::to_string),
                        (bits & 1 != 0).then(|| "owner".to_owned()),
                        "{extension} bits {bits} ignore {ignore}"
                    );
                    assert_eq!(
                        rebuilt.metadata.schema.as_deref(),
                        (bits & 2 != 0).then_some("schema.json")
                    );
                    assert_eq!(
                        rebuilt.metadata.ignore_unknown_instances,
                        bits & 4 != 0 && ignore
                    );
                    assert_eq!(rebuilt.class_name, "Folder");
                    assert!(rebuilt.children.is_empty());
                }
            }
        }
    }

    #[test]
    fn metadata_output_selects_one_format_and_clears_existing_values() {
        for path in ["/src/Twin.meta.json", "/src/Twin/init.meta.json"] {
            let path = Path::new(path);
            let jsonc = path.with_extension("jsonc");
            for files in 0..4 {
                for empty in [false, true] {
                    let mut imfs = InMemoryFs::new();
                    imfs.load_snapshot(path.parent().unwrap(), VfsSnapshot::empty_dir())
                        .unwrap();
                    for (bit, candidate) in [(1, path), (2, jsonc.as_path())] {
                        if files & bit != 0 {
                            imfs.load_snapshot(
                                candidate,
                                VfsSnapshot::file(r#"{"name":"Twin","attributes":{"Stale":1}}"#),
                            )
                            .unwrap();
                        }
                    }
                    let vfs = Vfs::new(imfs);
                    let mut output = FsSnapshot::new();
                    let metadata = if empty {
                        serde_json::json!({})
                    } else {
                        serde_json::json!({"name":"twin", "id":"owner",
                            "$schema":"schema.json", "ignoreUnknownInstances":true})
                    };
                    let result = write_metadata(&vfs, &mut output, path, &metadata, empty);
                    if files == 3 {
                        assert!(result.unwrap_err().to_string().contains("Both"));
                        assert!(output.is_empty());
                        continue;
                    }
                    result.unwrap();
                    assert!(output.removed_paths().is_empty());
                    if files == 0 && empty {
                        assert!(output.is_empty());
                        continue;
                    }
                    let target = if files == 2 { jsonc.as_path() } else { path };
                    assert_eq!(output.added_files(), [target]);
                    output.write_to_vfs("/", &vfs).unwrap();
                    let mut snapshot = InstanceSnapshot::new().name("Twin");
                    if path.ends_with("init.meta.json") {
                        DirectoryMetadata::read_and_apply_all(
                            &vfs,
                            path.parent().unwrap(),
                            &mut snapshot,
                        )
                        .unwrap();
                    } else {
                        AdjacentMetadata::read_and_apply_all(
                            &vfs,
                            &path.with_extension("luau"),
                            "Twin",
                            &mut snapshot,
                        )
                        .unwrap();
                    }
                    assert_eq!(snapshot.name, if empty { "Twin" } else { "twin" });
                    assert!(!snapshot.properties.contains_key(&ustr("Attributes")));
                    assert_eq!(snapshot.metadata.ignore_unknown_instances, !empty);
                    assert_eq!(snapshot.metadata.specified_id.is_some(), !empty);
                    assert_eq!(snapshot.metadata.schema.is_some(), !empty);
                }
            }
        }
    }

    #[test]
    fn metadata_output_keeps_scoped_and_ignored_ownership() {
        use crate::{
            snapshot::RojoTree,
            snapshot_middleware::Middleware,
            syncback::{syncback_instance, validate_write_targets},
            Project,
        };
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        for (middleware, extension, class, property, value, initializer) in [
            (
                Middleware::ModuleScript,
                "luau",
                "ModuleScript",
                "Source",
                "return 1",
                false,
            ),
            (
                Middleware::ModuleScriptDir,
                "luau",
                "ModuleScript",
                "Source",
                "return 1",
                true,
            ),
            (
                Middleware::Text,
                "txt",
                "StringValue",
                "Value",
                "text",
                false,
            ),
            (
                Middleware::Csv,
                "csv",
                "LocalizationTable",
                "Contents",
                "[]",
                false,
            ),
            (
                Middleware::CsvDir,
                "csv",
                "LocalizationTable",
                "Contents",
                "[]",
                true,
            ),
        ] {
            for (dual, ignored) in [(false, false), (false, true), (true, false)] {
                let temp = tempfile::tempdir().unwrap();
                let vfs = Vfs::new_default().unwrap();
                vfs.set_watch_enabled(false);
                let project_path = temp.path().join("default.project.json");
                let mut project = serde_json::json!({"name":"Test","tree":{"$className":"Folder"}});
                if ignored {
                    project["syncbackRules"] =
                        serde_json::json!({"ignorePaths":["**/*.meta.jsonc"]});
                }
                vfs.write(&project_path, serde_json::to_vec(&project).unwrap())
                    .unwrap();
                let project = Project::load_exact(&vfs, &project_path, Some("Test")).unwrap();
                let path = temp.path().join(if initializer {
                    "Twin".to_owned()
                } else {
                    format!("Twin.{extension}")
                });
                let meta_path = if initializer {
                    vfs.create_dir_all(&path).unwrap();
                    path.join("init.meta.jsonc")
                } else {
                    temp.path().join("Twin.meta.jsonc")
                };
                let source = if initializer {
                    path.join(format!("init.{extension}"))
                } else {
                    path.clone()
                };
                vfs.write(
                    &source,
                    if extension == "csv" {
                        "Key,Source,Context,Example\n"
                    } else {
                        value
                    },
                )
                .unwrap();
                let original = r#"{"id":"owner","$schema":"schema.json","ignoreUnknownInstances":true,"attributes":{"Stale":1}}"#;
                vfs.write(&meta_path, original).unwrap();
                if dual {
                    vfs.write(meta_path.with_extension("json"), "{}").unwrap();
                }
                let mut root = InstanceSnapshot::new()
                    .name("Twin")
                    .class_name(class)
                    .property(ustr(property), value)
                    .metadata(
                        InstanceMetadata::new()
                            .middleware(middleware)
                            .instigating_source(path.clone())
                            .relevant_paths(vec![source.clone(), meta_path.clone()]),
                    );
                if initializer {
                    DirectoryMetadata::read_and_apply_all(&vfs, &path, &mut root).unwrap();
                } else {
                    AdjacentMetadata::read_and_apply_all(&vfs, &path, "Twin", &mut root).unwrap();
                }
                let tree = RojoTree::new(root);
                let incoming = WeakDom::new(
                    InstanceBuilder::new(class)
                        .with_name("Twin")
                        .with_property(property, value),
                );
                let result = syncback_instance(&vfs, &tree, &project, tree.get_root_id(), incoming);
                if dual {
                    assert!(result.err().unwrap().to_string().contains("Both"));
                    assert_eq!(vfs.read_to_string(&meta_path).unwrap().as_str(), original);
                    continue;
                }
                let output = result.unwrap();
                assert!(output.removed_paths().is_empty());
                assert!(!output
                    .added_files()
                    .contains(&meta_path.with_extension("json").as_path()));
                if ignored {
                    assert!(validate_write_targets(&project, &output).is_err());
                    assert_eq!(vfs.read_to_string(&meta_path).unwrap().as_str(), original);
                    continue;
                }
                validate_write_targets(&project, &output).unwrap();
                let expected =
                    FsSnapshot::capture_file_contents(&[source, meta_path.clone()], &vfs).unwrap();
                output
                    .write_single_file_to_vfs(temp.path(), &vfs, &expected)
                    .unwrap();
                let updated: serde_json::Value =
                    serde_json::from_slice(&vfs.read(&meta_path).unwrap()).unwrap();
                assert_eq!(updated["id"], "owner");
                assert_eq!(updated["$schema"], "schema.json");
                assert_eq!(updated["ignoreUnknownInstances"], true);
                assert!(updated.get("attributes").is_none());
            }
        }
    }

    #[test]
    fn model_root_metadata_keeps_only_what_the_model_cannot_hold() {
        let path = Path::new("/Model.meta.json");
        let renamed = |existing: Option<&str>, name: Option<&str>| {
            AdjacentMetadata::for_model_root(
                existing.map(str::as_bytes),
                path,
                name.map(str::to_owned),
                None,
                &IndexMap::new(),
            )
            .unwrap()
            .map(|contents| String::from_utf8(contents).unwrap())
        };

        // Nothing to store means no file at all.
        assert_eq!(renamed(None, None), None);
        assert_eq!(renamed(Some(r#"{ "name": "Old" }"#), None), None);

        let created = renamed(None, Some("New")).unwrap();
        assert!(created.contains(r#""name": "New""#), "{created}");

        // Values the model file stores are all that a sidecar loses.
        assert_eq!(
            renamed(
                Some(r#"{ "properties": { "Archivable": false }, "attributes": { "A": 1 } }"#),
                None
            ),
            None
        );

        // Every other field survives a new name and a removed one.
        let existing = r#"{
            "$schema": "schema.json",
            "id": "model",
            "ignoreUnknownInstances": true,
            "name": "Old",
            "properties": { "Archivable": false },
            "attributes": { "Amount": 1 }
        }"#;
        for name in [Some("New"), None] {
            let updated = renamed(Some(existing), name).unwrap();
            for kept in ["$schema", r#""id": "model""#, "ignoreUnknownInstances"] {
                assert!(updated.contains(kept), "{updated}");
            }
            for dropped in ["Old", "Archivable", "Amount"] {
                assert!(!updated.contains(dropped), "{updated}");
            }
            assert_eq!(
                updated.contains(r#""name": "New""#),
                name.is_some(),
                "{updated}"
            );
        }

        // The pure helper gives the same answer every time.
        assert_eq!(
            renamed(Some(existing), Some("New")),
            renamed(Some(existing), Some("New"))
        );
    }

    #[test]
    fn metadata_ids_defer_to_valid_attribute_ids() {
        let id = RojoRef::new("abc".into());
        let attributes_with =
            |value: UnresolvedValue| IndexMap::from([(REF_ID_ATTRIBUTE_NAME.to_owned(), value)]);

        // Attribute IDs are authoritative even when they replace the old metadata ID.
        let carried = [
            attributes_with(UnresolvedValue::FullyQualified(Variant::String(
                "abc".into(),
            ))),
            attributes_with(UnresolvedValue::Ambiguous(AmbiguousValue::String(
                "abc".into(),
            ))),
            attributes_with(UnresolvedValue::FullyQualified(Variant::String(
                "different".into(),
            ))),
            attributes_with(UnresolvedValue::Ambiguous(AmbiguousValue::String(
                "different".into(),
            ))),
            attributes_with(UnresolvedValue::FullyQualified(Variant::BinaryString(
                b"different".to_vec().into(),
            ))),
        ];
        for attributes in &carried {
            assert_eq!(metadata_id(Some(&id), attributes), None);
        }

        // Otherwise the ID has nowhere else to go, so it's kept.
        let elsewhere = [
            IndexMap::new(),
            attributes_with(UnresolvedValue::FullyQualified(Variant::Bool(true))),
            attributes_with(UnresolvedValue::FullyQualified(Variant::BinaryString(
                vec![0xff].into(),
            ))),
        ];
        for attributes in &elsewhere {
            assert_eq!(metadata_id(Some(&id), attributes).as_deref(), Some("abc"));
        }

        assert_eq!(metadata_id(None, &carried[0]), None);
    }

    #[test]
    fn existing_key_order_follows_the_file_including_jsonc() {
        let order = ExistingKeyOrder::parse(
            r#"{
                // Comments and trailing commas are allowed.
                "className": "Part",
                "properties": { "Locked": true, "Anchored": true, "Size": [1, 2, 3], },
                "attributes": { "Zeta": 1, "Alpha": 2 },
            }"#,
        )
        .unwrap();
        assert_eq!(order.properties, ["Locked", "Anchored", "Size"]);
        assert_eq!(order.attributes, ["Zeta", "Alpha"]);

        assert_eq!(
            ExistingKeyOrder::parse(r#"{ "className": "Folder" }"#),
            Some(ExistingKeyOrder::default())
        );
        assert_eq!(ExistingKeyOrder::parse("[]"), None);
        assert_eq!(ExistingKeyOrder::parse("{ not json"), None);
    }

    #[test]
    fn existing_key_order_keeps_old_keys_in_place_and_new_keys_last() {
        let order = ExistingKeyOrder {
            properties: vec!["Locked".into(), "Anchored".into()],
            attributes: vec!["Zeta".into(), "Alpha".into()],
        };
        let mut properties: IndexMap<Ustr, UnresolvedValue> =
            ["Size", "Anchored", "Color", "Locked"]
                .into_iter()
                .map(|key| {
                    (
                        ustr(key),
                        UnresolvedValue::from_variant_unambiguous(Variant::Bool(true)),
                    )
                })
                .collect();
        let mut attributes: IndexMap<String, UnresolvedValue> = ["Alpha", "New", "Zeta"]
            .into_iter()
            .map(|key| {
                (
                    key.to_owned(),
                    UnresolvedValue::from_variant_unambiguous(Variant::Bool(true)),
                )
            })
            .collect();

        order.apply(&mut properties, &mut attributes);

        let property_keys: Vec<_> = properties.keys().map(|key| key.as_str()).collect();
        let attribute_keys: Vec<_> = attributes.keys().map(String::as_str).collect();
        assert_eq!(property_keys, ["Locked", "Anchored", "Size", "Color"]);
        assert_eq!(attribute_keys, ["Zeta", "Alpha", "New"]);
    }

    /// Returns every ordering of `items`, standing in for the orders a hash
    /// map can iterate in.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut result = Vec::new();
        for index in 0..items.len() {
            let mut rest = items.to_vec();
            let first = rest.remove(index);
            for mut tail in permutations(&rest) {
                tail.insert(0, first.clone());
                result.push(tail);
            }
        }
        result
    }

    #[test]
    fn serialized_values_are_sorted_whatever_the_input_order() {
        let mut attrs = Attributes::new();
        attrs.insert("Zeta".into(), Variant::Bool(true));
        attrs.insert("Alpha".into(), Variant::Bool(true));
        let attrs = Variant::Attributes(attrs);
        let anchored = Variant::Bool(true);
        let transparency = Variant::Float32(0.5);
        let texture_content = Variant::String("a".into());
        let texture_pack_content = Variant::String("b".into());
        let values = [
            (ustr("Transparency"), &transparency),
            (ustr("TexturePackContent"), &texture_pack_content),
            (ustr("Attributes"), &attrs),
            (ustr("TextureContent"), &texture_content),
            (ustr("Anchored"), &anchored),
        ];

        let orderings = permutations(&values);
        assert_eq!(orderings.len(), 120, "every ordering of five values");
        for ordering in orderings {
            let (properties, attributes) = serialize_values("Part", ordering);
            let property_keys: Vec<_> = properties.keys().map(|key| key.as_str()).collect();
            let attribute_keys: Vec<_> = attributes.keys().map(String::as_str).collect();
            assert_eq!(
                property_keys,
                [
                    "Anchored",
                    "TextureContent",
                    "TexturePackContent",
                    "Transparency"
                ]
            );
            assert_eq!(attribute_keys, ["Alpha", "Zeta"]);
        }
    }

    #[test]
    fn new_keys_follow_existing_keys_in_sorted_order() {
        let order = ExistingKeyOrder {
            properties: vec!["Locked".into(), "Anchored".into()],
            attributes: Vec::new(),
        };
        let value = Variant::Bool(true);
        let names = ["Size", "Anchored", "Color", "Locked"];
        for ordering in permutations(&names) {
            let (mut properties, mut attributes) = serialize_values(
                "Part",
                ordering.into_iter().map(|name| (ustr(name), &value)),
            );
            order.apply(&mut properties, &mut attributes);
            let property_keys: Vec<_> = properties.keys().map(|key| key.as_str()).collect();
            assert_eq!(property_keys, ["Locked", "Anchored", "Color", "Size"]);
        }
    }

    #[test]
    fn directory_class_name_serializes_first() {
        let metadata = DirectoryMetadata {
            class_name: Some(ustr("AudioListener")),
            schema: Some("https://example.invalid/schema.json".into()),
            id: None,
            ignore_unknown_instances: None,
            name: None,
            properties: IndexMap::new(),
            attributes: IndexMap::new(),
            children: Vec::new(),
            path: PathBuf::new(),
        };
        let serialized = crate::json::to_string_pretty(&metadata).unwrap();
        assert!(serialized.starts_with("{\n    \"className\": \"AudioListener\""));
        assert!(serialized.contains("\n    \"$schema\":"));
    }

    #[test]
    fn adjacent_read_json() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo/bar.meta.json",
            VfsSnapshot::file(r#"{"id": "manually specified"}"#),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let path = Path::new("/foo/bar.rojo");
        let mut snapshot = InstanceSnapshot::new();

        AdjacentMetadata::read_and_apply_all(&vfs, path, "bar", &mut snapshot).unwrap();

        insta::assert_yaml_snapshot!(snapshot);
    }

    #[test]
    fn adjacent_read_jsonc() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo/bar.meta.jsonc",
            VfsSnapshot::file(r#"{"id": "manually specified"}"#),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let path = Path::new("/foo/bar.rojo");
        let mut snapshot = InstanceSnapshot::new();

        AdjacentMetadata::read_and_apply_all(&vfs, path, "bar", &mut snapshot).unwrap();

        insta::assert_yaml_snapshot!(snapshot);
    }

    #[test]
    fn directory_read_json() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo/init.meta.json",
            VfsSnapshot::file(r#"{"id": "manually specified"}"#),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let path = Path::new("/foo/");
        let mut snapshot = InstanceSnapshot::new();

        DirectoryMetadata::read_and_apply_all(&vfs, path, &mut snapshot).unwrap();

        insta::assert_yaml_snapshot!(snapshot);
    }

    #[test]
    fn directory_read_jsonc() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo/init.meta.jsonc",
            VfsSnapshot::file(r#"{"id": "manually specified"}"#),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let path = Path::new("/foo/");
        let mut snapshot = InstanceSnapshot::new();

        DirectoryMetadata::read_and_apply_all(&vfs, path, &mut snapshot).unwrap();

        insta::assert_yaml_snapshot!(snapshot);
    }
}
