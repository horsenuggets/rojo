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
    resolution::UnresolvedValue,
    snapshot::{InstanceMetadata, InstanceSnapshot},
    syncback::{is_json_compatible, SyncbackSnapshot},
    variant_eq::variant_eq,
    RojoRef,
};

/// Splits serializable values into metadata properties and attributes.
///
/// Internal `RBX` attributes and values JSON cannot hold, like a Vector3 with
/// a NaN component, are left out so the file can always be read back.
pub fn serialize_values<'a>(
    class: &str,
    values: impl IntoIterator<Item = (Ustr, &'a Variant)>,
) -> (
    IndexMap<Ustr, UnresolvedValue>,
    IndexMap<String, UnresolvedValue>,
) {
    let mut properties = IndexMap::new();
    let mut attributes = IndexMap::new();
    for (name, value) in values {
        match value {
            Variant::Attributes(attrs) => {
                for (attr_name, attr_value) in attrs.iter() {
                    if attr_name.starts_with("RBX") || !is_json_compatible(attr_value) {
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
    matches!((old, new), (Ok(old), Ok(new)) if variant_eq(&old, &new))
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
        for (key, unresolved) in self.properties {
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
    schema: Option<String>,

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
        let mut properties = IndexMap::new();
        let mut attributes = IndexMap::new();
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
            .old_inst()
            .map(|inst| inst.metadata().ignore_unknown_instances)
            .unwrap_or_default();

        let schema = snapshot
            .old_inst()
            .and_then(|inst| inst.metadata().schema.clone());

        let class = &snapshot.new_inst().class;
        for (name, value) in snapshot.get_path_filtered_properties(snapshot.new).unwrap() {
            match value {
                Variant::Attributes(attrs) => {
                    for (attr_name, attr_value) in attrs.iter() {
                        // We (probably) don't want to preserve internal
                        // attributes, only user defined ones.
                        if attr_name.starts_with("RBX") || !is_json_compatible(attr_value) {
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
        if let Some(parent) = path.parent() {
            let meta_path = parent.join(format!("{}.meta.json", snapshot.file_stem()));
            ExistingMetadata::read(snapshot.vfs(), &meta_path).apply(
                class,
                &mut properties,
                &mut attributes,
            );
        }

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
            id: snapshot.old_inst().and_then(|inst| {
                inst.metadata()
                    .specified_id
                    .as_ref()
                    .map(ToString::to_string)
            }),
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

        // BTreeMaps don't have an equivalent to HashMap::drain, so the next
        // best option is to take ownership of the entire map. Not free, but
        // very cheap.
        for (key, unresolved) in std::mem::take(&mut self.properties) {
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
    /// - `ignore_unknown_instances` is None
    /// - `name` is None
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
            && self.properties.is_empty()
            && self.ignore_unknown_instances.is_none()
            && self.name.is_none()
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
            .old_inst()
            .map(|inst| inst.metadata().ignore_unknown_instances)
            .unwrap_or_default();

        let schema = snapshot
            .old_inst()
            .and_then(|inst| inst.metadata().schema.clone());

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
            id: snapshot.old_inst().and_then(|inst| {
                inst.metadata()
                    .specified_id
                    .as_ref()
                    .map(ToString::to_string)
            }),
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

        for (key, unresolved) in std::mem::take(&mut self.properties) {
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
    /// - `ignore_unknown_instances` is None
    /// - `name` is None
    /// - `class_name` is either None or not Some("Folder")
    /// - There are no inline children
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
            && self.properties.is_empty()
            && self.children.is_empty()
            && self.ignore_unknown_instances.is_none()
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
