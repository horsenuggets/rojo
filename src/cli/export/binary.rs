//! Reparents native model chunks without decoding or rewriting their property values.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, ensure, Context};
use rbx_dom_weak::{
    types::{ContentType, Ref, Variant, VariantType},
    InstanceBuilder, WeakDom,
};

use crate::web::export::{CaptureReferenceTarget, CaptureResult, CaptureRoot, Scope};

const MAGIC: &[u8; 14] = b"<roblox!\x89\xff\r\n\x1a\n";
const HEADER_LEN: usize = 32;
const MAX_ENCODED_BYTES: usize = 272 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 256 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 1024 * 1024 * 1024;
const MAX_INSTANCES: usize = 4 * 1024 * 1024;
const MAX_CHUNKS: usize = 65536;

struct Chunk<'a> {
    name: [u8; 4],
    encoded: &'a [u8],
    data: Vec<u8>,
}

struct Class {
    name: String,
    referents: Vec<i32>,
}

struct Model<'a> {
    chunks: Vec<Chunk<'a>>,
    classes: HashMap<u32, Class>,
    instances: HashMap<i32, u32>,
    names: HashMap<i32, Vec<u8>>,
    parents: Vec<(i32, i32)>,
    roots: Vec<i32>,
    subtree_sizes: HashMap<i32, usize>,
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, count: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(
            count <= self.remaining.len(),
            "The native binary capture is truncated."
        );
        let (bytes, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(bytes)
    }

    fn byte(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self) -> anyhow::Result<&'a [u8]> {
        let count = self.u32()? as usize;
        self.take(count)
    }

    fn finish(self) -> anyhow::Result<()> {
        ensure!(
            self.remaining.is_empty(),
            "A native binary structure has unexpected trailing data."
        );
        Ok(())
    }

    fn interleaved(&mut self, count: usize) -> anyhow::Result<Vec<u32>> {
        let size = count
            .checked_mul(4)
            .context("A native array length overflowed.")?;
        let bytes = self.take(size)?;
        Ok((0..count)
            .map(|index| {
                u32::from_be_bytes(std::array::from_fn(|lane| bytes[lane * count + index]))
            })
            .collect())
    }

    fn references(&mut self, count: usize) -> anyhow::Result<Vec<i32>> {
        let mut previous = 0_i32;
        self.interleaved(count)?
            .into_iter()
            .map(|value| {
                let delta = ((value >> 1) as i32) ^ -((value & 1) as i32);
                previous = previous.wrapping_add(delta);
                Ok(previous)
            })
            .collect()
    }
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_string(output: &mut Vec<u8>, value: &[u8]) -> anyhow::Result<()> {
    write_u32(output, value.len().try_into()?);
    output.extend_from_slice(value);
    Ok(())
}

fn write_interleaved(output: &mut Vec<u8>, values: &[u32]) {
    for lane in 0..4 {
        for value in values {
            output.push(value.to_be_bytes()[lane]);
        }
    }
}

fn write_references(output: &mut Vec<u8>, values: impl IntoIterator<Item = i32>) {
    let mut previous = 0_i32;
    let values: Vec<_> = values
        .into_iter()
        .map(|value| {
            let delta = value.wrapping_sub(previous);
            previous = value;
            ((delta as u32) << 1) ^ ((delta >> 31) as u32)
        })
        .collect();
    write_interleaved(output, &values);
}

fn append_chunk(output: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) -> anyhow::Result<()> {
    ensure!(
        data.len() <= MAX_CHUNK_BYTES,
        "A reconstructed native chunk exceeds the size limit."
    );
    let compressed = lz4_flex::block::compress(data);
    output.extend_from_slice(name);
    write_u32(output, compressed.len().try_into()?);
    write_u32(output, data.len().try_into()?);
    write_u32(output, 0);
    output.extend_from_slice(&compressed);
    Ok(())
}

fn empty_model() -> anyhow::Result<Vec<u8>> {
    let dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, &[])?;
    Ok(bytes)
}

impl<'a> Model<'a> {
    fn parse(bytes: &'a [u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() <= MAX_ENCODED_BYTES,
            "The native binary capture exceeds the size limit."
        );
        let mut input = Reader::new(bytes);
        ensure!(
            input.take(MAGIC.len())? == MAGIC,
            "The native binary capture has an invalid magic header."
        );
        ensure!(
            input.take(2)? == [0, 0],
            "The native binary capture uses an unsupported file version."
        );
        let expected_classes = input.u32()? as usize;
        let expected_instances = input.u32()? as usize;
        ensure!(
            expected_instances <= MAX_INSTANCES,
            "The native instance count exceeds the size limit."
        );
        ensure!(
            input.take(8)? == [0; 8],
            "The native binary capture has nonzero reserved header bytes."
        );
        let mut chunks = Vec::new();
        let mut decoded_size = 0_usize;
        loop {
            ensure!(
                chunks.len() < MAX_CHUNKS,
                "The native chunk count exceeds the size limit."
            );
            let encoded_start = input.remaining;
            let name = input.take(4)?.try_into().unwrap();
            let compressed_len = input.u32()? as usize;
            let decoded_len = input.u32()? as usize;
            ensure!(
                input.u32()? == 0,
                "A native chunk has nonzero reserved bytes."
            );
            ensure!(
                decoded_len <= MAX_CHUNK_BYTES,
                "A native chunk exceeds the decoded size limit."
            );
            decoded_size = decoded_size
                .checked_add(decoded_len)
                .context("Native decoded lengths overflowed.")?;
            ensure!(
                decoded_size <= MAX_DECODED_BYTES,
                "The native capture exceeds the decoded size limit."
            );
            let encoded_len = if compressed_len == 0 {
                decoded_len
            } else {
                compressed_len
            };
            let payload = input.take(encoded_len)?;
            let data = if compressed_len == 0 {
                payload.to_vec()
            } else if payload.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                zstd::bulk::decompress(payload, decoded_len)
                    .context("Could not decompress a native Zstandard chunk")?
            } else {
                lz4_flex::block::decompress(payload, decoded_len)
                    .context("Could not decompress a native LZ4 chunk")?
            };
            ensure!(
                data.len() == decoded_len,
                "A native chunk's decoded length does not match its header."
            );
            let encoded = &encoded_start[..encoded_start.len() - input.remaining.len()];
            chunks.push(Chunk {
                name,
                encoded,
                data,
            });
            if &name == b"END\0" {
                ensure!(
                    chunks.last().unwrap().data == b"</roblox>",
                    "The native END chunk is invalid."
                );
                input.finish()?;
                break;
            }
        }

        let mut classes = HashMap::new();
        let mut instances = HashMap::new();
        for chunk in &chunks {
            if &chunk.name != b"INST" {
                continue;
            }
            let mut reader = Reader::new(&chunk.data);
            let id = reader.u32()?;
            let name = String::from_utf8(reader.string()?.to_vec())
                .context("A native class name is not UTF-8")?;
            let format = reader.byte()?;
            ensure!(
                format <= 1,
                "A native INST chunk uses an unsupported object format."
            );
            let count = reader.u32()? as usize;
            ensure!(
                count <= expected_instances.saturating_sub(instances.len()),
                "Native INST counts exceed the header instance count."
            );
            let referents = reader.references(count)?;
            if format == 1 {
                ensure!(
                    reader.take(count)?.iter().all(|flag| *flag == 1),
                    "A native service marker is invalid."
                );
            }
            reader.finish()?;
            for referent in &referents {
                ensure!(*referent >= 0, "A native instance has a negative referent.");
                ensure!(
                    instances.insert(*referent, id).is_none(),
                    "The native capture contains a duplicate instance referent."
                );
            }
            ensure!(
                classes.insert(id, Class { name, referents }).is_none(),
                "The native capture contains a duplicate type ID."
            );
        }
        ensure!(
            classes.len() == expected_classes && instances.len() == expected_instances,
            "Native INST declarations do not match the file header counts."
        );

        let mut names = HashMap::new();
        let mut properties = HashSet::new();
        let mut references = Vec::new();
        let mut declared_types = HashSet::new();
        for chunk in &chunks {
            if &chunk.name == b"INST" {
                declared_types.insert(Reader::new(&chunk.data).u32()?);
                continue;
            }
            if &chunk.name != b"PROP" {
                continue;
            }
            let mut reader = Reader::new(&chunk.data);
            let type_id = reader.u32()?;
            let class = classes
                .get(&type_id)
                .context("A native property refers to an unknown type ID")?;
            ensure!(
                declared_types.contains(&type_id),
                "A native property precedes its INST declaration."
            );
            let name = reader.string()?;
            ensure!(
                properties.insert((type_id, name.to_vec())),
                "The native capture contains a duplicate property declaration."
            );
            let tag = if reader.remaining.is_empty() {
                None
            } else {
                Some(reader.byte()?)
            };
            if name == b"Name" {
                ensure!(
                    tag == Some(0x01),
                    "A native Name property must use the string type."
                );
                for referent in &class.referents {
                    names.insert(*referent, reader.string()?.to_vec());
                }
                reader.finish()?;
            } else if tag == Some(0x13) {
                references.extend(reader.references(class.referents.len())?);
                reader.finish()?;
            }
        }
        for referent in references {
            ensure!(
                referent == -1 || instances.contains_key(&referent),
                "A native reference property has a dangling target."
            );
        }

        let parent_chunks: Vec<_> = chunks
            .iter()
            .filter(|chunk| &chunk.name == b"PRNT")
            .collect();
        ensure!(
            parent_chunks.len() == 1,
            "The native capture must contain exactly one PRNT chunk."
        );
        let mut reader = Reader::new(&parent_chunks[0].data);
        ensure!(
            reader.byte()? == 0,
            "The native PRNT chunk uses an unsupported version."
        );
        let count = reader.u32()? as usize;
        ensure!(
            count == expected_instances,
            "The native PRNT count does not match the instance count."
        );
        let subjects = reader.references(count)?;
        let targets = reader.references(count)?;
        reader.finish()?;
        let parents: Vec<_> = subjects.into_iter().zip(targets).collect();
        let mut parent_by_id = HashMap::with_capacity(count);
        let mut children = HashMap::<i32, usize>::new();
        let mut roots = Vec::new();
        for &(subject, parent) in &parents {
            ensure!(
                instances.contains_key(&subject),
                "The native parent graph contains an unknown subject."
            );
            ensure!(
                parent == -1 || instances.contains_key(&parent),
                "The native parent graph contains a dangling parent."
            );
            ensure!(
                parent_by_id.insert(subject, parent).is_none(),
                "The native parent graph contains a duplicate subject."
            );
            if parent == -1 {
                roots.push(subject);
            } else {
                *children.entry(parent).or_default() += 1;
            }
        }
        let mut subtree_sizes: HashMap<_, _> = instances.keys().map(|id| (*id, 1_usize)).collect();
        let mut leaves: VecDeque<_> = instances
            .keys()
            .filter(|id| !children.contains_key(*id))
            .copied()
            .collect();
        let mut visited = 0;
        while let Some(id) = leaves.pop_front() {
            visited += 1;
            let parent = parent_by_id[&id];
            if parent != -1 {
                let size = subtree_sizes[&id];
                *subtree_sizes.get_mut(&parent).unwrap() += size;
                let remaining = children.get_mut(&parent).unwrap();
                *remaining -= 1;
                if *remaining == 0 {
                    leaves.push_back(parent);
                }
            }
        }
        ensure!(
            visited == count,
            "The native parent graph contains a cycle."
        );
        Ok(Self {
            chunks,
            classes,
            instances,
            names,
            parents,
            roots,
            subtree_sizes,
        })
    }

    fn validate_roots(
        &self,
        roots: &[CaptureRoot],
        scope: Scope,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        ensure!(
            self.roots.len() == roots.len(),
            "Studio omitted or added capture roots. The export cannot be safely reconstructed."
        );
        // The native serializer keeps capture roots in input order, including identical
        // labels. StudioExport's native contract test pins that order across permutations.
        for (&id, root) in self.roots.iter().zip(roots) {
            let class = &self.classes[&self.instances[&id]].name;
            let name = self
                .names
                .get(&id)
                .context("A native capture root has no serialized Name property")?;
            ensure!(
                class == &root.class_name && name == root.name.as_bytes(),
                "Studio's capture root order does not match the manifest."
            );
            ensure!(
                scope != Scope::Model || root.service.is_none(),
                "The capture root does not match the requested export scope."
            );
            let count = self.subtree_sizes[&id] - 1;
            if count != root.descendant_count {
                warnings.push(format!(
                    "Studio serialized {count} of {} expected descendants of \"{}\".",
                    root.descendant_count, root.name
                ));
            }
        }
        Ok(())
    }
}

fn property_header<'a>(data: &'a [u8]) -> anyhow::Result<(u32, &'a [u8], Option<u8>, Reader<'a>)> {
    let mut reader = Reader::new(data);
    let type_id = reader.u32()?;
    let name = reader.string()?;
    let tag = if reader.remaining.is_empty() {
        None
    } else {
        Some(reader.byte()?)
    };
    Ok((type_id, name, tag, reader))
}

fn supported_tag(tag: u8) -> bool {
    matches!(tag, 0x01..=0x0e | 0x10 | 0x12..=0x1c | 0x1e..=0x22)
}

fn xml_text(bytes: &[u8]) -> anyhow::Result<&str> {
    let text = std::str::from_utf8(bytes).context("Native text is not valid UTF-8")?;
    ensure!(
        text.chars().all(|character| matches!(character,
            '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}')),
        "Native text contains characters XML cannot preserve."
    );
    Ok(text)
}

fn xml_property_type(class: &str, name: &str, tag: u8) -> anyhow::Result<(String, VariantType)> {
    use rbx_reflection::{PropertyKind, PropertySerialization};
    use VariantType::*;

    let default = match tag {
        0x01 => BinaryString,
        0x02 => Bool,
        0x03 => Int32,
        0x04 => Float32,
        0x05 => Float64,
        0x06 => UDim,
        0x07 => UDim2,
        0x08 => Ray,
        0x09 => Faces,
        0x0a => Axes,
        0x0b => BrickColor,
        0x0c => Color3,
        0x0d => Vector2,
        0x0e => Vector3,
        0x10 => CFrame,
        0x12 => Enum,
        0x13 => Ref,
        0x14 => Vector3int16,
        0x15 => NumberSequence,
        0x16 => ColorSequence,
        0x17 => NumberRange,
        0x18 => Rect,
        0x19 => PhysicalProperties,
        0x1a => Color3uint8,
        0x1b => Int64,
        0x1c => SharedString,
        0x1e => OptionalCFrame,
        0x1f => UniqueId,
        0x20 => Font,
        0x21 => SecurityCapabilities,
        0x22 => Content,
        _ => bail!("The native property uses an unsupported binary type."),
    };
    if name == "Name" {
        ensure!(tag == 0x01, "The native Name property is not a string.");
        return Ok((name.to_owned(), String));
    }

    let database =
        rbx_reflection_database::get().context("Could not load the reflection database")?;
    let descriptor = database.classes.get(class).and_then(|class| {
        database
            .superclasses_iter(class)
            .find_map(|class| class.properties.get(name).map(|property| (class, property)))
    });
    let (canonical_name, ty) = if let Some((class, property)) = descriptor {
        let canonical = match &property.kind {
            PropertyKind::Canonical { .. } => property,
            PropertyKind::Alias { alias_for } => class
                .properties
                .get(*alias_for)
                .context("The native property alias has no canonical descriptor")?,
            _ => bail!("The native property has an unsupported reflection descriptor."),
        };
        match &canonical.kind {
            PropertyKind::Canonical {
                serialization: PropertySerialization::Serializes,
            } => {
                ensure!(
                    canonical.name == name,
                    "The native property uses a nonserialized alias."
                );
            }
            PropertyKind::Canonical {
                serialization: PropertySerialization::SerializesAs(serialized),
            } => {
                ensure!(
                    *serialized == name,
                    "The native property uses a nonserialized name."
                );
            }
            _ => bail!("Typed conversion skips or migrates the native property."),
        }
        (canonical.name.to_string(), canonical.data_type.ty())
    } else {
        (name.to_owned(), default)
    };
    let compatible = match tag {
        0x01 => matches!(
            ty,
            String | BinaryString | ContentId | Tags | Attributes | MaterialColors
        ),
        0x03 => matches!(ty, Int32 | Int64),
        0x1a => ty == Color3,
        0x1c => matches!(ty, SharedString | NetAssetRef | Tags),
        _ => ty == default,
    };
    ensure!(
        compatible,
        "Typed conversion cannot preserve the native property's type."
    );
    Ok((canonical_name, ty))
}

fn xml_string(bytes: &[u8], ty: VariantType) -> anyhow::Result<()> {
    use rbx_dom_weak::types::MaterialColors;
    match ty {
        VariantType::String | VariantType::ContentId => {
            xml_text(bytes)?;
        }
        VariantType::Tags => {
            // The decoder removes empty tags. Validate without allocating one String per tag.
            ensure!(
                bytes.iter().filter(|byte| **byte == 0).count() < MAX_INSTANCES,
                "The native XML tag list exceeds the entry limit."
            );
            for tag in bytes.split(|byte| *byte == 0) {
                ensure!(
                    !tag.is_empty() || bytes.is_empty(),
                    "Typed conversion removes empty tags."
                );
                xml_text(tag)?;
            }
        }
        VariantType::Attributes => {
            // Nonempty blobs have a second, unbounded parser in the pinned dependency.
            ensure!(
                bytes.is_empty() || bytes == [0; 4],
                "XML conversion cannot safely validate nonempty native Attributes blobs."
            );
        }
        VariantType::MaterialColors => {
            let colors = MaterialColors::decode(bytes)?;
            ensure!(
                colors.encode() == bytes,
                "Typed conversion discards native material-color data."
            );
        }
        _ => {}
    }
    Ok(())
}

fn xml_array(reader: &mut Reader<'_>, count: usize, width: usize) -> anyhow::Result<()> {
    reader.take(
        count
            .checked_mul(width)
            .context("A native array length overflowed")?,
    )?;
    Ok(())
}

fn xml_cframes(reader: &mut Reader<'_>, count: usize) -> anyhow::Result<()> {
    use rbx_dom_weak::types::Matrix3;
    for _ in 0..count {
        let rotation = reader.byte()?;
        if rotation == 0 {
            reader.take(36)?;
        } else {
            Matrix3::from_basic_rotation_id(rotation)?;
        }
    }
    xml_array(reader, count, 12)
}

fn xml_property_payload(
    mut reader: Reader<'_>,
    tag: u8,
    ty: VariantType,
    count: usize,
    strings: &[&[u8]],
    available_instances: &HashSet<i32>,
) -> anyhow::Result<()> {
    use rbx_dom_weak::types::{BrickColor, FontStyle, FontWeight};
    match tag {
        0x01 => {
            for _ in 0..count {
                xml_string(reader.string()?, ty)?;
            }
        }
        0x02 | 0x09 | 0x0a => {
            let maximum = match tag {
                0x02 => 1,
                0x09 => 0x3f,
                _ => 0x07,
            };
            ensure!(
                reader.take(count)?.iter().all(|byte| *byte <= maximum),
                "The native property contains invalid bit values."
            );
        }
        0x0b => {
            for value in reader.interleaved(count)? {
                ensure!(
                    u16::try_from(value)
                        .ok()
                        .and_then(BrickColor::from_number)
                        .is_some(),
                    "The native property has an invalid BrickColor."
                );
            }
        }
        0x03 | 0x04 | 0x12 | 0x13 => xml_array(&mut reader, count, 4)?,
        0x05 | 0x06 | 0x0d | 0x17 | 0x1b | 0x21 => xml_array(&mut reader, count, 8)?,
        0x07 | 0x18 | 0x1f => xml_array(&mut reader, count, 16)?,
        0x08 => xml_array(&mut reader, count, 24)?,
        0x0c | 0x0e => xml_array(&mut reader, count, 12)?,
        0x10 => xml_cframes(&mut reader, count)?,
        0x14 => xml_array(&mut reader, count, 6)?,
        0x15 | 0x16 => {
            for _ in 0..count {
                let keypoints = reader.u32()? as usize;
                let width = if tag == 0x15 { 12 } else { 20 };
                ensure!(
                    keypoints <= reader.remaining.len() / width,
                    "The native sequence count exceeds its payload."
                );
                let data = reader.take(keypoints * width)?;
                if tag == 0x16 {
                    ensure!(
                        data.as_chunks::<20>()
                            .0
                            .iter()
                            .all(|point| point[16..20] == [0; 4]),
                        "Typed conversion discards native ColorSequence envelopes."
                    );
                }
            }
        }
        0x19 => {
            for _ in 0..count {
                match reader.byte()? {
                    0 => {}
                    1 => {
                        reader.take(20)?;
                    }
                    3 => {
                        reader.take(24)?;
                    }
                    _ => bail!(
                        "Typed conversion cannot preserve the native physical-properties mode."
                    ),
                }
            }
        }
        0x1a => xml_array(&mut reader, count, 3)?,
        0x1c => {
            for index in reader.interleaved(count)? {
                let bytes = strings
                    .get(index as usize)
                    .context("The native shared-string index is not declared before use")?;
                if ty == VariantType::Tags {
                    xml_string(bytes, ty)?;
                }
            }
        }
        0x1e => {
            ensure!(
                reader.byte()? == 0x10,
                "The native optional CFrame marker is invalid."
            );
            xml_cframes(&mut reader, count)?;
            ensure!(
                reader.byte()? == 0x02,
                "The native optional CFrame presence marker is invalid."
            );
            ensure!(
                reader.take(count)?.iter().all(|byte| *byte <= 1),
                "The native optional CFrame has invalid presence flags."
            );
        }
        0x20 => {
            for _ in 0..count {
                xml_text(reader.string()?)?;
                let weight = u16::from_le_bytes(reader.take(2)?.try_into().unwrap());
                ensure!(
                    FontWeight::from_u16(weight).is_some(),
                    "The native font weight is invalid."
                );
                ensure!(
                    FontStyle::from_u8(reader.byte()?).is_some(),
                    "The native font style is invalid."
                );
                xml_text(reader.string()?)?;
            }
        }
        0x22 => {
            let types = reader.interleaved(count)?;
            // These are zig-zagged signed integers, not raw discriminators.
            ensure!(
                types.iter().all(|ty| matches!(ty, 0 | 2 | 4)),
                "The native Content source type is invalid."
            );
            let uris = reader.u32()? as usize;
            ensure!(
                uris == types.iter().filter(|ty| **ty == 2).count(),
                "The native Content URI count does not match its source types."
            );
            for _ in 0..uris {
                xml_text(reader.string()?)?;
            }
            let objects = reader.u32()? as usize;
            ensure!(
                objects == types.iter().filter(|ty| **ty == 4).count(),
                "The native Content object count does not match its source types."
            );
            let references = reader.references(objects)?;
            ensure!(
                references.iter().all(|id| available_instances.contains(id)),
                "Typed conversion would discard an unresolved native Content reference."
            );
            // The pinned decoder consumes object references in reverse order.
            ensure!(
                references.iter().eq(references.iter().rev()),
                "Typed conversion would reorder native Content references."
            );
            ensure!(
                reader.u32()? == 0,
                "Typed conversion discards external native Content references."
            );
        }
        _ => bail!("The native property uses an unsupported binary type."),
    }
    reader.finish()
}

fn validate_xml_capture(bytes: &[u8]) -> anyhow::Result<()> {
    let model = Model::parse(bytes)?;
    let mut strings = Vec::new();
    let mut declared_classes = HashSet::new();
    let mut available_instances = HashSet::new();
    let mut canonical_properties = HashSet::new();
    for chunk in &model.chunks {
        match &chunk.name {
            b"INST" => {
                let mut reader = Reader::new(&chunk.data);
                let id = reader.u32()?;
                xml_text(model.classes[&id].name.as_bytes())?;
                declared_classes.insert(id);
                available_instances.extend(&model.classes[&id].referents);
            }
            b"SSTR" => {
                let shared = shared_strings(&chunk.data)?;
                ensure!(
                    shared.count as usize <= MAX_INSTANCES - strings.len(),
                    "The native XML shared-string table exceeds the entry limit."
                );
                let mut reader = Reader::new(shared.entries);
                for _ in 0..shared.count {
                    reader.take(16)?;
                    strings.push(reader.string()?);
                }
            }
            b"PROP" => {
                let (id, name, tag, reader) = property_header(&chunk.data)?;
                ensure!(
                    declared_classes.contains(&id),
                    "The native property precedes its class declaration."
                );
                let class = &model.classes[&id];
                let name = xml_text(name)?;
                let tag = tag.context("The native property has no binary type")?;
                let (canonical, ty) =
                    xml_property_type(&class.name, name, tag).with_context(|| {
                        format!(
                            "Cannot preserve native property \"{}.{}\"",
                            class.name, name
                        )
                    })?;
                ensure!(
                    canonical_properties.insert((id, canonical)),
                    "Multiple native properties overwrite the same canonical value."
                );
                xml_property_payload(
                    reader,
                    tag,
                    ty,
                    class.referents.len(),
                    &strings,
                    &available_instances,
                )
                .with_context(|| {
                    format!(
                        "Cannot preserve native property \"{}.{}\"",
                        class.name, name
                    )
                })?;
            }
            b"PRNT" => ensure!(
                declared_classes.len() == model.classes.len(),
                "The native parent graph precedes its instance declarations."
            ),
            b"END\0" => {}
            b"META" => bail!("XML conversion cannot preserve native META chunks."),
            _ => bail!(
                "XML conversion cannot preserve native chunk {:?}.",
                String::from_utf8_lossy(&chunk.name)
            ),
        }
    }
    Ok(())
}

/// Validates nested lengths and reflection behavior before invoking the pinned decoder.
pub(super) fn ensure_xml_safe(bytes: &[u8]) -> anyhow::Result<()> {
    validate_xml_capture(bytes).context(
        "XML conversion cannot safely preserve this capture. Export a binary model or place instead.",
    )
}

struct SharedStrings<'a> {
    count: u32,
    entries: &'a [u8],
}

fn shared_strings(data: &[u8]) -> anyhow::Result<SharedStrings<'_>> {
    let mut reader = Reader::new(data);
    ensure!(
        reader.u32()? == 0,
        "Cannot merge an unsupported native shared-string version."
    );
    let count = reader.u32()?;
    let entries = reader.remaining;
    // Each entry needs a hash and a length, even for an empty string.
    ensure!(
        count as usize <= reader.remaining.len() / 20,
        "The native shared-string count exceeds its payload."
    );
    for _ in 0..count {
        reader.take(16)?;
        reader.string()?;
    }
    reader.finish()?;
    Ok(SharedStrings { count, entries })
}

fn remap_referent(id: i32, mapping: &HashMap<i32, i32>) -> anyhow::Result<i32> {
    if id == -1 {
        Ok(-1)
    } else {
        mapping
            .get(&id)
            .copied()
            .context("A synthesized property has an unmapped reference target")
    }
}

fn remap_property(
    chunk: &Chunk<'_>,
    classes: &HashMap<u32, Class>,
    type_ids: &HashMap<u32, u32>,
    referents: &HashMap<i32, i32>,
    shared_offset: u32,
    reference_targets: &mut HashMap<(i32, String), i32>,
) -> anyhow::Result<Vec<u8>> {
    let (old_type, name, tag, mut reader) = property_header(&chunk.data)?;
    let tag = tag.context("A synthesized property has no binary type")?;
    ensure!(
        supported_tag(tag),
        "A synthesized property uses an unsupported binary type."
    );
    let count = classes
        .get(&old_type)
        .context("A synthesized property has an unknown class")?
        .referents
        .len();
    let mut output = Vec::new();
    write_u32(&mut output, type_ids[&old_type]);
    write_string(&mut output, name)?;
    output.push(tag);
    match tag {
        0x13 => {
            let ids = reader
                .references(count)?
                .into_iter()
                .enumerate()
                .map(|(index, id)| {
                    let key = (classes[&old_type].referents[index], String::from_utf8(name.to_vec())?);
                    match reference_targets.remove(&key) {
                        Some(target) => {
                            ensure!(id == -1, "A capture-relative reference replaced a non-null synthesized value.");
                            Ok(target)
                        }
                        None => remap_referent(id, referents),
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            reader.finish()?;
            write_references(&mut output, ids);
        }
        0x1c => {
            let ids = reader
                .interleaved(count)?
                .into_iter()
                .map(|id| {
                    id.checked_add(shared_offset)
                        .context("A synthesized shared-string index overflowed")
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            reader.finish()?;
            write_interleaved(&mut output, &ids);
        }
        0x22 => {
            // Content can contain instance references in addition to URI strings.
            let source_types = reader.interleaved(count)?;
            write_interleaved(&mut output, &source_types);
            let uri_count = reader.u32()?;
            ensure!(
                uri_count as usize <= count,
                "A synthesized Content URI count exceeds its instance count."
            );
            write_u32(&mut output, uri_count);
            for _ in 0..uri_count {
                write_string(&mut output, reader.string()?)?;
            }
            let object_count = reader.u32()?;
            ensure!(
                object_count as usize <= count,
                "A synthesized Content object count exceeds its instance count."
            );
            write_u32(&mut output, object_count);
            let ids = reader
                .references(object_count as usize)?
                .into_iter()
                .map(|id| remap_referent(id, referents))
                .collect::<anyhow::Result<Vec<_>>>()?;
            write_references(&mut output, ids);
            ensure!(
                reader.u32()? == 0,
                "Cannot remap synthesized external Content references."
            );
            write_u32(&mut output, 0);
            reader.finish()?;
        }
        _ => output.extend_from_slice(reader.remaining),
    }
    Ok(output)
}

/// Preserves native binary values, referents and opaque chunks while adding services.
pub(super) fn assemble_capture(
    capture: CaptureResult,
    scope: Scope,
) -> anyhow::Result<(Vec<u8>, Vec<String>)> {
    let manifest = capture.manifest;
    if let Some(error) = manifest.error {
        bail!("Studio capture failed. {error}");
    }
    let mut warnings = manifest.warnings;
    let original = if capture.model_contents.is_empty() {
        empty_model()?
    } else {
        capture.model_contents
    };
    let model = Model::parse(&original)?;
    model.validate_roots(&manifest.roots, scope, &mut warnings)?;
    if scope == Scope::Model {
        ensure!(
            manifest.services.is_empty(),
            "A model capture unexpectedly contains service containers."
        );
        return Ok((original, warnings));
    }
    if manifest.services.is_empty() {
        ensure!(
            manifest.roots.iter().all(|root| root.service.is_none()),
            "The capture root refers to an unknown service."
        );
        return Ok((original, warnings));
    }

    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let service_order: Vec<_> = manifest
        .services
        .iter()
        .map(|service| service.name.clone())
        .collect();
    let mut pending_references = Vec::new();
    let mut service_names = HashSet::new();
    for service in manifest.services {
        ensure!(
            service_names.insert(service.name.clone()),
            "The capture contains duplicate service names."
        );
        let mut reference_names = HashSet::new();
        for reference in &service.references {
            let database =
                rbx_reflection_database::get().context("Could not load the reflection database")?;
            let class = database
                .classes
                .get(service.class_name.as_str())
                .context("A service reference has an unknown source class")?;
            let descriptor = database
                .superclasses_iter(class)
                .find_map(|class| class.properties.get(reference.property.as_str()))
                .context("A service reference has an unknown source property")?;
            ensure!(
                descriptor.data_type.ty() == VariantType::Ref,
                "A captured service reference targets a non-reference property."
            );
            ensure!(
                !reference.property.is_empty()
                    && reference.property != "Name"
                    && reference.property != "Parent"
                    && reference_names.insert(reference.property.clone())
                    && !service.properties.contains_key(&reference.property),
                "A captured service reference has a duplicate, conflicting, or reserved property name."
            );
        }
        let mut builder = InstanceBuilder::new(service.class_name).with_name(&service.name);
        for (name, value) in service.properties {
            if name == "Name" {
                ensure!(
                    matches!(&value, Variant::String(name) if name == &service.name),
                    "A captured service Name property disagrees with its manifest name."
                );
                continue;
            }
            if matches!(&value, Variant::Ref(target) if !target.is_none()) {
                warnings.push(format!("Omitted service reference property \"{}.{}\", which has no capture-relative target.", service.name, name));
                continue;
            }
            if let Variant::Content(content) = &value {
                ensure!(!matches!(content.value(), ContentType::Object(_)), "Service object-content property \"{}.{}\" has no capture-relative target mapping.", service.name, name);
            }
            builder = builder.with_property(name, value);
        }
        for reference in service.references {
            // Native targets do not exist in this temporary DOM. Patch these null
            // placeholders only after all synthesized service IDs are allocated.
            builder = builder.with_property(&reference.property, Ref::none());
            pending_references.push((service.name.clone(), reference));
        }
        dom.insert(dom.root_ref(), builder);
    }
    let mut service_bytes = Vec::new();
    rbx_binary::to_writer(&mut service_bytes, &dom, dom.root().children())
        .context("Could not serialize captured service properties")?;
    let synthesized = Model::parse(&service_bytes)?;
    let mut next_type = match model.classes.keys().max() {
        Some(id) => id
            .checked_add(1)
            .context("Native type IDs exhausted the available range")?,
        None => 0,
    };
    let mut type_ids = HashMap::new();
    for chunk in &synthesized.chunks {
        if &chunk.name == b"INST" {
            let id = Reader::new(&chunk.data).u32()?;
            type_ids.insert(id, next_type);
            next_type = next_type
                .checked_add(1)
                .context("Synthesized type IDs exhausted the available range")?;
        }
    }
    let mut next_ref = match model.instances.keys().max() {
        Some(id) => id
            .checked_add(1)
            .context("Native referents exhausted the available range")?,
        None => 0,
    };
    let mut referents = HashMap::new();
    for &(id, _) in &synthesized.parents {
        referents.insert(id, next_ref);
        next_ref = next_ref
            .checked_add(1)
            .context("Synthesized referents exhausted the available range")?;
    }
    let mut service_ids = HashMap::new();
    for &id in &synthesized.roots {
        let name = synthesized
            .names
            .get(&id)
            .context("A synthesized service has no serialized name")?;
        service_ids.insert(String::from_utf8(name.clone())?, referents[&id]);
    }
    ensure!(
        service_ids.len() == service_names.len(),
        "Synthesized service roots do not match the manifest."
    );

    let mut reference_targets = HashMap::new();
    for (service, reference) in pending_references {
        let target = match reference.target {
            CaptureReferenceTarget::Root { index } => *model
                .roots
                .get(index)
                .context("A service reference points outside the captured roots")?,
            CaptureReferenceTarget::Service { index } => {
                let name = service_order
                    .get(index)
                    .context("A service reference points outside the captured services")?;
                service_ids[name]
            }
        };
        let source = synthesized
            .roots
            .iter()
            .find(|id| synthesized.names[id] == service.as_bytes())
            .context("A captured reference source service was not serialized")?;
        reference_targets.insert((*source, reference.property), target);
    }

    let old_sstr: Vec<_> = model
        .chunks
        .iter()
        .filter(|chunk| &chunk.name == b"SSTR")
        .collect();
    let new_sstr: Vec<_> = synthesized
        .chunks
        .iter()
        .filter(|chunk| &chunk.name == b"SSTR")
        .collect();
    let mut merged_sstr = None;
    let mut shared_offset = 0;
    if !new_sstr.is_empty() {
        ensure!(
            old_sstr.len() <= 1 && new_sstr.len() == 1,
            "Cannot merge multiple native shared-string tables safely."
        );
        let added = shared_strings(&new_sstr[0].data)?;
        let existing = old_sstr
            .first()
            .map(|chunk| shared_strings(&chunk.data))
            .transpose()?;
        shared_offset = existing.as_ref().map_or(0, |strings| strings.count);
        let count = shared_offset
            .checked_add(added.count)
            .context("The merged shared-string count overflowed")?;
        let mut bytes = Vec::new();
        write_u32(&mut bytes, 0);
        write_u32(&mut bytes, count);
        if let Some(existing) = existing {
            bytes.extend_from_slice(existing.entries);
        }
        bytes.extend_from_slice(added.entries);
        merged_sstr = Some(bytes);
    }

    let mut added_chunks = Vec::new();
    for chunk in &synthesized.chunks {
        match &chunk.name {
            b"INST" => {
                let mut reader = Reader::new(&chunk.data);
                let id = reader.u32()?;
                let name = reader.string()?;
                let format = reader.byte()?;
                let count = reader.u32()? as usize;
                let ids = reader.references(count)?;
                let mut data = Vec::new();
                write_u32(&mut data, type_ids[&id]);
                write_string(&mut data, name)?;
                data.push(format);
                write_u32(&mut data, count.try_into()?);
                write_references(&mut data, ids.into_iter().map(|id| referents[&id]));
                data.extend_from_slice(reader.remaining);
                append_chunk(&mut added_chunks, b"INST", &data)?;
            }
            b"PROP" => append_chunk(
                &mut added_chunks,
                b"PROP",
                &remap_property(
                    chunk,
                    &synthesized.classes,
                    &type_ids,
                    &referents,
                    shared_offset,
                    &mut reference_targets,
                )?,
            )?,
            b"SSTR" | b"PRNT" | b"END\0" => {}
            _ => bail!(
                "Cannot safely combine synthesized chunk {:?} with the native model.",
                String::from_utf8_lossy(&chunk.name)
            ),
        }
    }

    ensure!(
        reference_targets.is_empty(),
        "A captured service reference was not serialized as an instance-reference property."
    );

    let mut reparent = HashMap::new();
    for (&id, root) in model.roots.iter().zip(&manifest.roots) {
        if let Some(service) = &root.service {
            reparent.insert(
                id,
                *service_ids
                    .get(service)
                    .context("The capture root refers to an unknown service")?,
            );
        }
    }
    let mut parents = model.parents.clone();
    for (id, parent) in &mut parents {
        if let Some(service) = reparent.get(id) {
            ensure!(
                *parent == -1,
                "Only native capture roots can be reparented."
            );
            *parent = *service;
        }
    }
    for (id, parent) in &synthesized.parents {
        parents.push((referents[id], remap_referent(*parent, &referents)?));
    }
    let mut parent_data = vec![0];
    write_u32(&mut parent_data, parents.len().try_into()?);
    write_references(&mut parent_data, parents.iter().map(|(id, _)| *id));
    write_references(&mut parent_data, parents.iter().map(|(_, parent)| *parent));

    let mut output = original[..HEADER_LEN].to_vec();
    let count = model
        .classes
        .len()
        .checked_add(synthesized.classes.len())
        .context("The reconstructed type count overflowed")?;
    output[16..20].copy_from_slice(&u32::try_from(count)?.to_le_bytes());
    output[20..24].copy_from_slice(&u32::try_from(parents.len())?.to_le_bytes());
    if old_sstr.is_empty() {
        if let Some(data) = &merged_sstr {
            append_chunk(&mut output, b"SSTR", data)?;
        }
    }
    for chunk in &model.chunks {
        match &chunk.name {
            b"PRNT" => {
                output.extend_from_slice(&added_chunks);
                append_chunk(&mut output, b"PRNT", &parent_data)?;
            }
            b"SSTR" if merged_sstr.is_some() => {
                append_chunk(&mut output, b"SSTR", merged_sstr.as_ref().unwrap())?
            }
            _ => output.extend_from_slice(chunk.encoded),
        }
        ensure!(
            output.len() <= MAX_ENCODED_BYTES,
            "The reconstructed binary file exceeds the size limit."
        );
    }
    // Validate the rewritten identity and parent graph as strictly as the input.
    Model::parse(&output).context("The reconstructed native binary graph is invalid")?;
    Ok((output, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::export::{CaptureManifest, CaptureService};
    use rbx_dom_weak::types::{Ref, SharedString, UniqueId};

    fn encode(dom: &WeakDom) -> Vec<u8> {
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, dom, dom.root().children()).unwrap();
        bytes
    }

    fn fixture(scope: Scope) -> CaptureResult {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder").with_name("Root").with_child(
                InstanceBuilder::new("StringValue")
                    .with_name("Child")
                    .with_property("Value", "untouched"),
            ),
        );
        CaptureResult {
            model_contents: encode(&dom),
            manifest: CaptureManifest {
                part_count: 1,
                roots: vec![CaptureRoot {
                    name: "Root".into(),
                    class_name: "Folder".into(),
                    service: (scope == Scope::Place).then(|| "Workspace".into()),
                    descendant_count: 1,
                }],
                services: if scope == Scope::Place {
                    vec![CaptureService {
                        name: "Workspace".into(),
                        class_name: "Workspace".into(),
                        properties: HashMap::from([("Gravity".into(), Variant::Float32(123.0))]),
                        references: vec![],
                    }]
                } else {
                    vec![]
                },
                warnings: vec![],
                error: None,
            },
        }
    }

    fn modify_chunks(bytes: &[u8], mut modify: impl FnMut(&Chunk<'_>, &mut Vec<u8>)) -> Vec<u8> {
        let model = Model::parse(bytes).unwrap();
        let mut output = bytes[..HEADER_LEN].to_vec();
        for chunk in &model.chunks {
            modify(chunk, &mut output);
        }
        output
    }

    fn inject(bytes: &[u8], name: &[u8; 4], data: &[u8]) -> Vec<u8> {
        modify_chunks(bytes, |chunk, output| {
            if &chunk.name == b"PRNT" {
                append_chunk(output, name, data).unwrap();
            }
            output.extend_from_slice(chunk.encoded);
        })
    }

    fn opaque_property(bytes: &[u8], property: &str, tag: u8, payload: &[u8]) -> Vec<u8> {
        let model = Model::parse(bytes).unwrap();
        let id = model.instances[&model.roots[0]];
        let mut data = Vec::new();
        write_u32(&mut data, id);
        write_string(&mut data, property.as_bytes()).unwrap();
        data.push(tag);
        data.extend_from_slice(payload);
        inject(bytes, b"PROP", &data)
    }

    fn metadata_payload() -> Vec<u8> {
        let mut data = Vec::new();
        write_u32(&mut data, 1);
        write_string(&mut data, b"ExplicitAutoJoints").unwrap();
        write_string(&mut data, b"true").unwrap();
        data
    }

    fn asset_content_map_payload(bundle: Option<&[u8]>, entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut data = vec![
            (if bundle.is_some() { 1 } else { 0 })
                | (if !entries.is_empty() || bundle.is_none() {
                    2
                } else {
                    0
                }),
        ];
        if let Some(bundle) = bundle {
            write_string(&mut data, bundle).unwrap();
        }
        if !entries.is_empty() || bundle.is_none() {
            data.extend_from_slice(&(entries.len() as i64).to_le_bytes());
            for (key, value) in entries {
                write_string(&mut data, key).unwrap();
                write_string(&mut data, value).unwrap();
            }
        }
        data
    }

    fn terrain_capture(scope: Scope, payload: &[u8]) -> CaptureResult {
        let mut capture = fixture(scope);
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Terrain")
                .with_name("Terrain")
                .with_child(InstanceBuilder::new("Clouds")),
        );
        capture.model_contents =
            opaque_property(&encode(&dom), "VoxelGridAssetContentMap", 0x23, payload);
        capture.manifest.roots[0].name = "Terrain".into();
        capture.manifest.roots[0].class_name = "Terrain".into();
        capture
    }

    #[test]
    fn model_captures_are_byte_identical_including_opaque_values_and_metadata() {
        let payload = asset_content_map_payload(None, &[(b"L3;0,0,0;N4", b"VXFG\0\xff")]);
        let mut capture = terrain_capture(Scope::Model, &payload);
        capture.model_contents = inject(&capture.model_contents, b"FUTR", &[0, 255, 128, 1]);
        capture.model_contents = inject(&capture.model_contents, b"META", &metadata_payload());
        let expected = capture.model_contents.clone();
        let (actual, warnings) = assemble_capture(capture, Scope::Model).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(actual, expected);
    }

    #[test]
    fn place_assembly_preserves_all_original_nonparent_chunks_exactly() {
        let payload = asset_content_map_payload(None, &[(b"L3;0,0,0;N4", b"VXFG\0\xff")]);
        let mut capture = terrain_capture(Scope::Place, &payload);
        capture.model_contents = inject(&capture.model_contents, b"META", &metadata_payload());
        capture.model_contents = inject(&capture.model_contents, b"FUTR", &[9, 8, 7]);
        let expected = Model::parse(&capture.model_contents)
            .unwrap()
            .chunks
            .iter()
            .filter(|chunk| &chunk.name != b"PRNT")
            .map(|chunk| chunk.encoded.to_vec())
            .collect::<Vec<_>>();
        let (actual, warnings) = assemble_capture(capture, Scope::Place).unwrap();
        assert!(warnings.is_empty());
        let parsed = Model::parse(&actual).unwrap();
        for encoded in expected {
            assert!(parsed.chunks.iter().any(|chunk| chunk.encoded == encoded));
        }
        assert_eq!(parsed.instances.len(), 3);
        assert_eq!(parsed.roots.len(), 1);
        assert_eq!(parsed.names[&parsed.roots[0]], b"Workspace");
        let dom = rbx_binary::from_reader(actual.as_slice()).unwrap();
        let workspace = dom
            .descendants()
            .find(|instance| instance.class == "Workspace")
            .unwrap();
        assert_eq!(
            workspace.properties[&rbx_dom_weak::ustr("Gravity")],
            Variant::Float32(123.0)
        );
    }

    #[test]
    fn expanded_terrain_maps_keep_empty_bundle_and_binary_entry_payloads() {
        for payload in [
            asset_content_map_payload(None, &[]),
            asset_content_map_payload(Some(b"1234567890"), &[]),
            asset_content_map_payload(
                None,
                &[(b"L3;0,0,0;N4", b"VXFG\0\xff"), (b"L7;0,0,0;N4", b"")],
            ),
            asset_content_map_payload(Some(b"42"), &[(b"key", b"opaque\0\xff")]),
        ] {
            let capture = terrain_capture(Scope::Place, &payload);
            let original = Model::parse(&capture.model_contents).unwrap();
            let expected = original
                .chunks
                .iter()
                .find(|chunk| {
                    &chunk.name == b"PROP"
                        && property_header(&chunk.data).unwrap().1 == b"VoxelGridAssetContentMap"
                })
                .unwrap()
                .encoded
                .to_vec();
            let output = assemble_capture(capture, Scope::Place).unwrap().0;
            assert!(Model::parse(&output)
                .unwrap()
                .chunks
                .iter()
                .any(|chunk| chunk.encoded == expected));
        }
    }

    #[test]
    fn duplicate_names_cross_root_references_and_identity_properties_survive_reparenting() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let identity = UniqueId::new(7, 8, 9);
        let target = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Part")
                .with_name("Duplicate")
                .with_property("UniqueId", identity)
                .with_property("HistoryId", identity),
        );
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("ObjectValue")
                .with_name("Duplicate")
                .with_property("Value", target),
        );
        let mut capture = fixture(Scope::Place);
        capture.model_contents = encode(&dom);
        let parsed = Model::parse(&capture.model_contents).unwrap();
        capture.manifest.roots = parsed
            .roots
            .iter()
            .map(|id| CaptureRoot {
                name: "Duplicate".into(),
                class_name: parsed.classes[&parsed.instances[id]].name.clone(),
                service: Some("Workspace".into()),
                descendant_count: 0,
            })
            .collect();
        let original_ids: HashSet<_> = parsed.instances.keys().copied().collect();
        let output = assemble_capture(capture, Scope::Place).unwrap().0;
        let parsed = Model::parse(&output).unwrap();
        assert!(original_ids
            .iter()
            .all(|id| parsed.instances.contains_key(id)));
        let decoded = rbx_binary::from_reader(output.as_slice()).unwrap();
        let part = decoded
            .descendants()
            .find(|instance| instance.class == "Part")
            .unwrap();
        assert_eq!(
            part.properties[&rbx_dom_weak::ustr("UniqueId")],
            Variant::UniqueId(identity)
        );
        assert_eq!(
            part.properties[&rbx_dom_weak::ustr("HistoryId")],
            Variant::UniqueId(identity)
        );
        let reference = decoded
            .descendants()
            .find(|instance| instance.class == "ObjectValue")
            .unwrap();
        assert_eq!(
            reference.properties[&rbx_dom_weak::ustr("Value")],
            Variant::Ref(part.referent())
        );
    }

    #[test]
    fn service_references_resolve_native_roots_and_later_services_without_rewriting_native_chunks()
    {
        use crate::web::export::{CaptureReference, CaptureReferenceTarget};
        for target in [
            CaptureReferenceTarget::Root { index: 0 },
            CaptureReferenceTarget::Service { index: 1 },
        ] {
            let expected_class = match target {
                CaptureReferenceTarget::Root { .. } => "Folder",
                CaptureReferenceTarget::Service { .. } => "ServerStorage",
            };
            let mut capture = fixture(Scope::Place);
            capture.manifest.services[0]
                .references
                .push(CaptureReference {
                    property: "CurrentCamera".into(),
                    target,
                });
            capture.manifest.services.push(CaptureService {
                name: "ServerStorage".into(),
                class_name: "ServerStorage".into(),
                properties: HashMap::new(),
                references: vec![],
            });
            let original_chunks: Vec<_> = Model::parse(&capture.model_contents)
                .unwrap()
                .chunks
                .iter()
                .filter(|chunk| &chunk.name != b"PRNT")
                .map(|chunk| chunk.encoded.to_vec())
                .collect();
            let (bytes, warnings) = assemble_capture(capture, Scope::Place).unwrap();
            assert!(warnings.is_empty(), "{warnings:?}");
            let assembled = Model::parse(&bytes).unwrap();
            for encoded in original_chunks {
                assert!(assembled
                    .chunks
                    .iter()
                    .any(|chunk| chunk.encoded == encoded));
            }
            let decoded = rbx_binary::from_reader(bytes.as_slice()).unwrap();
            let workspace = decoded
                .descendants()
                .find(|instance| instance.class == "Workspace")
                .unwrap();
            let target = decoded
                .descendants()
                .find(|instance| instance.class == expected_class)
                .unwrap();
            assert_eq!(
                workspace
                    .properties
                    .get(&rbx_dom_weak::ustr("CurrentCamera")),
                Some(&Variant::Ref(target.referent()))
            );
        }
    }

    #[test]
    fn service_reference_mapping_rejects_invalid_conflicting_and_unserialized_targets() {
        use crate::web::export::{CaptureReference, CaptureReferenceTarget};
        for case in 0..7 {
            let mut capture = fixture(Scope::Place);
            let property = match case {
                4 => "NotAProperty",
                5 => "Gravity",
                6 => "Parent",
                _ => "CurrentCamera",
            };
            let target = if case == 1 {
                CaptureReferenceTarget::Service { index: 1 }
            } else {
                CaptureReferenceTarget::Root {
                    index: if case == 0 { 1 } else { 0 },
                }
            };
            capture.manifest.services[0]
                .references
                .push(CaptureReference {
                    property: property.into(),
                    target,
                });
            if case == 2 {
                capture.manifest.services[0]
                    .references
                    .push(CaptureReference {
                        property: property.into(),
                        target: CaptureReferenceTarget::Root { index: 0 },
                    });
            }
            if case == 3 {
                capture.manifest.services[0]
                    .properties
                    .insert(property.into(), Variant::Ref(Ref::none()));
            }
            assert!(
                assemble_capture(capture, Scope::Place).is_err(),
                "Case {case} was accepted"
            );
        }
    }

    #[test]
    fn service_name_properties_must_agree_with_the_manifest_name() {
        for value in [
            Variant::String("Workspace".into()),
            Variant::String("Other".into()),
            Variant::Int32(1),
        ] {
            let valid = value == Variant::String("Workspace".into());
            let mut capture = fixture(Scope::Place);
            capture.manifest.services[0]
                .properties
                .insert("Name".into(), value);
            assert_eq!(assemble_capture(capture, Scope::Place).is_ok(), valid);
        }
    }

    #[test]
    fn empty_service_only_captures_and_top_level_roots_are_supported() {
        let mut capture = fixture(Scope::Place);
        capture.model_contents.clear();
        capture.manifest.roots.clear();
        let bytes = assemble_capture(capture, Scope::Place).unwrap().0;
        assert_eq!(Model::parse(&bytes).unwrap().instances.len(), 1);
        let mut capture = fixture(Scope::Place);
        capture.manifest.roots[0].service = None;
        let bytes = assemble_capture(capture, Scope::Place).unwrap().0;
        assert_eq!(Model::parse(&bytes).unwrap().roots.len(), 2);
    }

    #[test]
    fn root_metadata_errors_and_scope_mismatches_fail() {
        let mut capture = fixture(Scope::Model);
        capture.manifest.roots[0].name = "Wrong".into();
        assert!(assemble_capture(capture, Scope::Model).is_err());
        let mut capture = fixture(Scope::Model);
        capture.manifest.roots.clear();
        assert!(assemble_capture(capture, Scope::Model).is_err());
        let mut capture = fixture(Scope::Place);
        capture.manifest.services.clear();
        assert!(assemble_capture(capture, Scope::Place).is_err());
        assert!(assemble_capture(fixture(Scope::Place), Scope::Model).is_err());
        let mut capture = fixture(Scope::Place);
        capture.manifest.services.push(CaptureService {
            name: "Workspace".into(),
            class_name: "Workspace".into(),
            properties: HashMap::new(),
            references: vec![],
        });
        assert!(assemble_capture(capture, Scope::Place).is_err());
    }

    #[test]
    fn descendant_count_differences_remain_visible() {
        let mut capture = fixture(Scope::Model);
        capture.manifest.roots[0].descendant_count += 1;
        let warnings = assemble_capture(capture, Scope::Model).unwrap().1;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("serialized 1 of 2"));
    }

    #[test]
    fn malformed_headers_and_chunks_return_errors_without_panicking() {
        let bytes = fixture(Scope::Model).model_contents;
        for length in [0, 1, 13, 14, 15, 16, 31, 32, bytes.len() - 1] {
            assert!(Model::parse(&bytes[..length]).is_err());
        }
        for (index, value) in [(0, 0), (14, 1), (24, 1), (44, 1)] {
            let mut malformed = bytes.clone();
            malformed[index] = value;
            assert!(Model::parse(&malformed).is_err());
        }
        let mut malformed = bytes.clone();
        malformed[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Model::parse(&malformed).is_err());
        let mut malformed = bytes.clone();
        malformed[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Model::parse(&malformed).is_err());
        let mut malformed = bytes.clone();
        malformed.extend_from_slice(b"trailing");
        assert!(Model::parse(&malformed).is_err());
    }

    #[test]
    fn truncated_compressed_streams_and_incorrect_decoded_lengths_fail() {
        let bytes = fixture(Scope::Model).model_contents;
        let bad = modify_chunks(&bytes, |chunk, output| {
            if &chunk.name == b"INST" {
                let mut data = chunk.encoded.to_vec();
                let length = u32::from_le_bytes(data[8..12].try_into().unwrap()) + 1;
                data[8..12].copy_from_slice(&length.to_le_bytes());
                output.extend_from_slice(&data);
            } else {
                output.extend_from_slice(chunk.encoded);
            }
        });
        assert!(Model::parse(&bad).is_err());
        let bad = modify_chunks(&bytes, |chunk, output| {
            if &chunk.name == b"INST" {
                output.extend_from_slice(b"INST");
                write_u32(output, 1);
                write_u32(output, 10);
                write_u32(output, 0);
                output.push(0xff);
            } else {
                output.extend_from_slice(chunk.encoded);
            }
        });
        assert!(Model::parse(&bad).is_err());
    }

    fn change_parents(bytes: &[u8], change: impl Fn(&mut Vec<(i32, i32)>)) -> Vec<u8> {
        let parsed = Model::parse(bytes).unwrap();
        let mut parents = parsed.parents;
        change(&mut parents);
        modify_chunks(bytes, |chunk, output| {
            if &chunk.name == b"PRNT" {
                let mut data = vec![0];
                write_u32(&mut data, parents.len() as u32);
                write_references(&mut data, parents.iter().map(|(id, _)| *id));
                write_references(&mut data, parents.iter().map(|(_, parent)| *parent));
                append_chunk(output, b"PRNT", &data).unwrap();
            } else {
                output.extend_from_slice(chunk.encoded);
            }
        })
    }

    #[test]
    fn parent_graph_rejects_dangling_duplicate_unknown_and_cyclic_nodes() {
        type ParentChange = fn(&mut Vec<(i32, i32)>);
        let bytes = fixture(Scope::Model).model_contents;
        let changes: [ParentChange; 4] = [
            |parents| parents[0].1 = i32::MAX,
            |parents| parents[0].0 = parents[1].0,
            |parents| parents[0].0 = i32::MAX,
            |parents| parents[0].1 = parents[0].0,
        ];
        for change in changes {
            assert!(Model::parse(&change_parents(&bytes, change)).is_err());
        }
    }

    #[test]
    fn duplicate_instance_declarations_and_dangling_property_references_fail() {
        let bytes = fixture(Scope::Model).model_contents;
        let bad = modify_chunks(&bytes, |chunk, output| {
            output.extend_from_slice(chunk.encoded);
            if &chunk.name == b"INST" {
                output.extend_from_slice(chunk.encoded);
            }
        });
        assert!(Model::parse(&bad).is_err());
        let mut payload = Vec::new();
        write_references(&mut payload, [i32::MAX]);
        let bad = opaque_property(&bytes, "UnmappedReference", 0x13, &payload);
        assert!(Model::parse(&bad).is_err());
    }

    #[test]
    fn shared_string_entries_are_appended_without_changing_native_indices() {
        let mut capture = fixture(Scope::Place);
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder")
                .with_name("Root")
                .with_property("NativeShared", SharedString::new(vec![0, 255, 7])),
        );
        capture.model_contents = encode(&dom);
        capture.manifest.roots[0].descendant_count = 0;
        capture.manifest.services[0].properties.insert(
            "ServiceShared".into(),
            Variant::SharedString(SharedString::new(vec![9, 8, 128])),
        );
        let original = Model::parse(&capture.model_contents).unwrap();
        let native_property = original
            .chunks
            .iter()
            .find(|chunk| {
                &chunk.name == b"PROP" && property_header(&chunk.data).unwrap().1 == b"NativeShared"
            })
            .unwrap()
            .encoded
            .to_vec();
        let original_strings = original
            .chunks
            .iter()
            .find(|chunk| &chunk.name == b"SSTR")
            .unwrap();
        let original_strings = shared_strings(&original_strings.data).unwrap();
        let original_count = original_strings.count;
        let original_entries = original_strings.entries.to_vec();
        let bytes = assemble_capture(capture, Scope::Place).unwrap().0;
        let parsed = Model::parse(&bytes).unwrap();
        assert!(parsed
            .chunks
            .iter()
            .any(|chunk| chunk.encoded == native_property));
        let strings = parsed
            .chunks
            .iter()
            .find(|chunk| &chunk.name == b"SSTR")
            .unwrap();
        let merged = shared_strings(&strings.data).unwrap();
        assert!(merged.count > original_count);
        assert!(merged.entries.starts_with(&original_entries));
        let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let folder = dom
            .descendants()
            .find(|instance| instance.class == "Folder")
            .unwrap();
        assert_eq!(
            folder.properties[&rbx_dom_weak::ustr("NativeShared")],
            Variant::SharedString(SharedString::new(vec![0, 255, 7]))
        );
        let workspace = dom
            .descendants()
            .find(|instance| instance.class == "Workspace")
            .unwrap();
        assert_eq!(
            workspace.properties[&rbx_dom_weak::ustr("ServiceShared")],
            Variant::SharedString(SharedString::new(vec![9, 8, 128]))
        );
    }

    #[test]
    fn a_new_shared_string_table_is_inserted_before_native_properties() {
        let mut capture = fixture(Scope::Place);
        capture.model_contents = modify_chunks(&capture.model_contents, |chunk, output| {
            if &chunk.name != b"SSTR"
                && !(&chunk.name == b"PROP"
                    && property_header(&chunk.data).unwrap().2 == Some(0x1c))
            {
                output.extend_from_slice(chunk.encoded);
            }
        });
        assert!(Model::parse(&capture.model_contents)
            .unwrap()
            .chunks
            .iter()
            .all(|chunk| &chunk.name != b"SSTR"));
        capture.manifest.services[0].properties.insert(
            "ServiceShared".into(),
            Variant::SharedString(SharedString::new(vec![1, 2, 3])),
        );
        let bytes = assemble_capture(capture, Scope::Place).unwrap().0;
        let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let workspace = dom
            .descendants()
            .find(|instance| instance.class == "Workspace")
            .unwrap();
        assert_eq!(
            workspace.properties[&rbx_dom_weak::ustr("ServiceShared")],
            Variant::SharedString(SharedString::new(vec![1, 2, 3]))
        );
    }

    fn xml_test_property(class: &str, name: &str, tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(dom.root_ref(), InstanceBuilder::new(class));
        let bytes = modify_chunks(&encode(&dom), |chunk, output| {
            if &chunk.name == b"PROP" && property_header(&chunk.data).unwrap().1 == name.as_bytes()
            {
                return;
            }
            output.extend_from_slice(chunk.encoded);
        });
        opaque_property(&bytes, name, tag, payload)
    }

    fn assert_xml_rejected(bytes: &[u8], reason: &str) {
        // Deliberately do not call the typed decoder on these adversarial payloads.
        let error = format!("{:#}", ensure_xml_safe(bytes).unwrap_err());
        assert!(
            error.contains("Export a binary model or place instead"),
            "{error}"
        );
        assert!(error.contains(reason), "{error}");
    }

    #[test]
    fn xml_rejects_lossy_text_but_preserves_arbitrary_binary_strings() {
        let mut payload = Vec::new();
        write_string(&mut payload, &[0xff, 0, 0x80]).unwrap();
        assert_xml_rejected(
            &xml_test_property("StringValue", "Value", 0x01, &payload),
            "UTF-8",
        );
        assert_xml_rejected(
            &xml_test_property("Folder", "Name", 0x01, &payload),
            "UTF-8",
        );
        let bytes = xml_test_property("Folder", "BinaryPayload", 0x01, &payload);
        ensure_xml_safe(&bytes).unwrap();
        let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let root = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(
            root.properties[&rbx_dom_weak::ustr("BinaryPayload")],
            Variant::BinaryString(vec![0xff, 0, 0x80].into())
        );
        let xml = super::super::encode_capture(&dom, super::super::OutputKind::Rbxmx).unwrap();
        let restored = rbx_xml::from_reader(
            xml.as_slice(),
            rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
        )
        .unwrap();
        let root = restored.get_by_ref(restored.root().children()[0]).unwrap();
        assert_eq!(
            root.properties[&rbx_dom_weak::ustr("BinaryPayload")],
            Variant::BinaryString(vec![0xff, 0, 0x80].into())
        );

        let mut payload = Vec::new();
        write_string(&mut payload, b"text\x01").unwrap();
        assert_xml_rejected(
            &xml_test_property("StringValue", "Value", 0x01, &payload),
            "characters XML",
        );
    }

    #[test]
    fn xml_rejects_reflection_skips_and_type_narrowing() {
        assert_xml_rejected(
            &xml_test_property("Part", "Position", 0x0e, &[0; 12]),
            "skips or migrates",
        );
        assert_xml_rejected(
            &xml_test_property("Part", "Transparency", 0x05, &[0; 8]),
            "property's type",
        );
    }

    #[test]
    fn xml_rejects_unbounded_nested_lengths_before_typed_decoding() {
        assert_xml_rejected(
            &xml_test_property("StringValue", "Value", 0x01, &u32::MAX.to_le_bytes()),
            "truncated",
        );
        for tag in [0x15, 0x16] {
            assert_xml_rejected(
                &xml_test_property("Folder", "Sequence", tag, &u32::MAX.to_le_bytes()),
                "sequence count",
            );
        }
        let mut content = Vec::new();
        write_interleaved(&mut content, &[2]);
        write_u32(&mut content, u32::MAX);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &content),
            "URI count",
        );
        content.truncate(4);
        write_u32(&mut content, 0);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &content),
            "URI count",
        );
        content.truncate(4);
        write_u32(&mut content, 1);
        write_u32(&mut content, u32::MAX);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &content),
            "truncated",
        );
    }

    #[test]
    fn xml_checks_content_counts_references_and_external_data() {
        let mut payload = Vec::new();
        write_interleaved(&mut payload, &[0]);
        write_u32(&mut payload, 0);
        write_u32(&mut payload, u32::MAX);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &payload),
            "object count",
        );
        payload.truncate(8);
        write_u32(&mut payload, 0);
        write_u32(&mut payload, 1);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &payload),
            "external",
        );
        payload.truncate(12);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &payload),
            "truncated",
        );

        payload.clear();
        write_interleaved(&mut payload, &[4]);
        write_u32(&mut payload, 0);
        write_u32(&mut payload, 1);
        write_references(&mut payload, [i32::MAX]);
        write_u32(&mut payload, 0);
        assert_xml_rejected(
            &xml_test_property("Folder", "Content", 0x22, &payload),
            "unresolved",
        );
    }

    #[test]
    fn xml_validates_shared_string_counts_lengths_and_indices() {
        let bytes = fixture(Scope::Model).model_contents;
        let mut table = Vec::new();
        write_u32(&mut table, 0);
        write_u32(&mut table, u32::MAX);
        assert_xml_rejected(&inject(&bytes, b"SSTR", &table), "count exceeds");
        table.truncate(4);
        write_u32(&mut table, 1);
        table.extend_from_slice(&[0; 16]);
        write_u32(&mut table, u32::MAX);
        assert_xml_rejected(&inject(&bytes, b"SSTR", &table), "truncated");
        let mut index = Vec::new();
        write_interleaved(&mut index, &[0]);
        assert_xml_rejected(
            &xml_test_property("Folder", "Shared", 0x1c, &index),
            "not declared",
        );
    }

    #[test]
    fn xml_validates_nested_markers_and_exact_property_lengths() {
        for (tag, payload, reason) in [
            (0x02, vec![2], "invalid bit values"),
            (0x03, vec![0; 5], "trailing data"),
            (0x10, vec![255], "rotation"),
            (0x1e, vec![0], "CFrame marker"),
            (0x19, vec![2], "physical-properties mode"),
        ] {
            assert_xml_rejected(
                &xml_test_property("Folder", "FutureValue", tag, &payload),
                reason,
            );
        }
        let mut font = Vec::new();
        write_string(&mut font, b"family").unwrap();
        font.extend_from_slice(&0_u16.to_le_bytes());
        font.push(0);
        write_string(&mut font, b"").unwrap();
        assert_xml_rejected(
            &xml_test_property("Folder", "FutureFont", 0x20, &font),
            "font weight",
        );
        let mut sequence = Vec::new();
        write_u32(&mut sequence, 1);
        sequence.extend_from_slice(&[0; 16]);
        sequence.extend_from_slice(&1_f32.to_le_bytes());
        assert_xml_rejected(
            &xml_test_property("Folder", "FutureSequence", 0x16, &sequence),
            "envelopes",
        );
    }

    #[test]
    fn xml_accepts_declared_shared_strings_and_rejects_out_of_range_indices() {
        let mut table = Vec::new();
        write_u32(&mut table, 0);
        write_u32(&mut table, 1);
        table.extend_from_slice(&[0; 16]);
        write_string(&mut table, &[0xff, 0x80]).unwrap();
        for index in [0, 1] {
            let mut payload = Vec::new();
            write_interleaved(&mut payload, &[index]);
            let bytes = xml_test_property("Folder", "Shared", 0x1c, &payload);
            let mut inserted = false;
            let bytes = modify_chunks(&bytes, |chunk, output| {
                if !inserted && &chunk.name == b"INST" {
                    append_chunk(output, b"SSTR", &table).unwrap();
                    inserted = true;
                }
                output.extend_from_slice(chunk.encoded);
            });
            if index == 0 {
                ensure_xml_safe(&bytes).unwrap();
                let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
                let instance = dom.get_by_ref(dom.root().children()[0]).unwrap();
                assert_eq!(
                    instance.properties[&rbx_dom_weak::ustr("Shared")],
                    Variant::SharedString(SharedString::new(vec![0xff, 0x80]))
                );
            } else {
                assert_xml_rejected(&bytes, "not declared");
            }
        }
    }

    #[test]
    fn xml_rejects_unvalidated_attribute_blobs_and_normalized_tags() {
        for data in [&[1, 0, 0, 0][..], &u32::MAX.to_le_bytes()[..]] {
            let mut payload = Vec::new();
            write_string(&mut payload, data).unwrap();
            assert_xml_rejected(
                &xml_test_property("Folder", "AttributesSerialize", 0x01, &payload),
                "nonempty native Attributes",
            );
        }
        let mut payload = Vec::new();
        write_string(&mut payload, b"first\0\0second").unwrap();
        assert_xml_rejected(
            &xml_test_property("Folder", "Tags", 0x01, &payload),
            "empty tags",
        );
        let error = xml_string(&vec![0; MAX_INSTANCES], VariantType::Tags).unwrap_err();
        assert!(error.to_string().contains("entry limit"));
    }

    #[test]
    fn xml_accepts_ordinary_text_content_and_supported_terrain_data() {
        let bytes = fixture(Scope::Model).model_contents;
        ensure_xml_safe(&bytes).unwrap();
        let mut content = Vec::new();
        write_interleaved(&mut content, &[2]);
        write_u32(&mut content, 1);
        write_string(&mut content, b"rbxassetid://123").unwrap();
        write_u32(&mut content, 0);
        write_u32(&mut content, 0);
        ensure_xml_safe(&xml_test_property("Folder", "Content", 0x22, &content)).unwrap();
        let mut colors = Vec::new();
        write_string(
            &mut colors,
            &rbx_dom_weak::types::MaterialColors::new().encode(),
        )
        .unwrap();
        let terrain = xml_test_property("Terrain", "MaterialColors", 0x01, &colors);
        ensure_xml_safe(&terrain).unwrap();
        let dom = rbx_binary::from_reader(terrain.as_slice()).unwrap();
        let mut xml = Vec::new();
        rbx_xml::to_writer_default(&mut xml, &dom, dom.root().children()).unwrap();
        assert!(!xml.is_empty());
    }

    #[test]
    fn xml_conversion_rejects_opaque_types_metadata_and_unknown_chunks() {
        let bytes = fixture(Scope::Model).model_contents;
        ensure_xml_safe(&bytes).unwrap();
        for tag in [0, 0x0f, 0x11, 0x1d, 0x23, 0xff] {
            assert!(
                ensure_xml_safe(&opaque_property(&bytes, "FutureValue", tag, &[1, 2])).is_err()
            );
        }
        assert!(ensure_xml_safe(&inject(&bytes, b"META", &[0, 0, 0, 0])).is_err());
        assert!(ensure_xml_safe(&inject(&bytes, b"FUTR", &[1, 2, 3])).is_err());
    }

    #[test]
    fn service_reference_omissions_remain_explicit_and_capture_errors_fail() {
        let mut capture = fixture(Scope::Place);
        capture.manifest.services[0]
            .properties
            .insert("CurrentCamera".into(), Variant::Ref(Ref::new()));
        let warnings = assemble_capture(capture, Scope::Place).unwrap().1;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Workspace.CurrentCamera"));
        let mut capture = fixture(Scope::Model);
        capture.manifest.error = Some("Capture interrupted.".into());
        assert!(assemble_capture(capture, Scope::Model).is_err());
    }

    #[test]
    fn captured_null_service_references_are_preserved_without_an_omission() {
        let mut capture = fixture(Scope::Place);
        capture.manifest.roots[0].service = Some("StarterGui".into());
        let service = &mut capture.manifest.services[0];
        service.name = "StarterGui".into();
        service.class_name = "StarterGui".into();
        service.properties =
            HashMap::from([("StudioDefaultStyleSheet".into(), Variant::Ref(Ref::none()))]);
        let (bytes, warnings) = assemble_capture(capture, Scope::Place).unwrap();
        assert!(warnings.is_empty());
        let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let service = dom
            .descendants()
            .find(|node| node.class == "StarterGui")
            .unwrap();
        assert_eq!(
            service.properties.get(&"StudioDefaultStyleSheet".into()),
            Some(&Variant::Ref(Ref::none()))
        );
    }

    #[test]
    fn service_object_content_cannot_silently_turn_into_a_null_reference() {
        let mut capture = fixture(Scope::Place);
        capture.manifest.services[0].properties.insert(
            "ObjectContent".into(),
            Variant::Content(rbx_dom_weak::types::Content::from_referent(Ref::new())),
        );
        assert!(assemble_capture(capture, Scope::Place).is_err());
    }

    #[test]
    fn identical_root_labels_keep_the_native_input_order_across_services() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        for index in 0..2 {
            dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("Folder")
                    .with_name("Duplicate")
                    .with_child(
                        InstanceBuilder::new("StringValue")
                            .with_name("Marker")
                            .with_property("Value", index.to_string()),
                    ),
            );
        }
        let mut capture = fixture(Scope::Place);
        capture.model_contents = encode(&dom);
        capture.manifest.services.push(CaptureService {
            name: "ServerStorage".into(),
            class_name: "ServerStorage".into(),
            properties: HashMap::new(),
            references: vec![],
        });
        capture.manifest.roots = ["Workspace", "ServerStorage"]
            .into_iter()
            .map(|service| CaptureRoot {
                name: "Duplicate".into(),
                class_name: "Folder".into(),
                service: Some(service.into()),
                descendant_count: 1,
            })
            .collect();
        let (bytes, warnings) = assemble_capture(capture, Scope::Place).unwrap();
        assert!(warnings.is_empty());
        let decoded = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        for (index, class) in ["Workspace", "ServerStorage"].into_iter().enumerate() {
            let service = decoded
                .descendants()
                .find(|node| node.class == class)
                .unwrap();
            assert_eq!(service.children().len(), 1);
            let folder = decoded.get_by_ref(service.children()[0]).unwrap();
            assert_eq!(folder.name, "Duplicate");
            let marker = decoded.get_by_ref(folder.children()[0]).unwrap();
            assert_eq!(
                marker.properties.get(&"Value".into()),
                Some(&Variant::String(index.to_string()))
            );
        }
    }

    #[test]
    #[ignore = "Requires ROJO_NATIVE_MODEL, ROJO_NATIVE_MANIFEST and a new ROJO_NATIVE_OUTPUT path"]
    fn captured_native_fixture_keeps_original_binary_properties_and_identifiers() {
        use std::io::Write;
        let required_path = |name| {
            std::path::PathBuf::from(
                std::env::var_os(name).expect("Set each native fixture path explicitly"),
            )
        };
        let source = std::fs::read(required_path("ROJO_NATIVE_MODEL")).unwrap();
        let manifest = serde_json::from_reader(
            std::fs::File::open(required_path("ROJO_NATIVE_MANIFEST")).unwrap(),
        )
        .unwrap();
        let original = Model::parse(&source).unwrap();
        let (bytes, _warnings) = assemble_capture(
            CaptureResult {
                model_contents: source.clone(),
                manifest,
            },
            Scope::Place,
        )
        .unwrap();
        let assembled = Model::parse(&bytes).unwrap();
        for (&id, &class) in &original.instances {
            assert_eq!(assembled.instances.get(&id), Some(&class));
        }
        for chunk in &original.chunks {
            if !matches!(&chunk.name, b"PRNT" | b"SSTR") {
                assert!(
                    assembled
                        .chunks
                        .iter()
                        .any(|other| other.encoded == chunk.encoded),
                    "An original native chunk was rewritten or lost"
                );
            }
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(required_path("ROJO_NATIVE_OUTPUT"))
            .unwrap();
        output.write_all(&bytes).unwrap();
        output.sync_all().unwrap();
    }

    #[test]
    fn zstandard_and_uncompressed_chunks_are_preserved_verbatim() {
        let capture = fixture(Scope::Model);
        for compressed in [false, true] {
            let bytes = modify_chunks(&capture.model_contents, |chunk, output| {
                let payload = if compressed {
                    zstd::bulk::compress(&chunk.data, 0).unwrap()
                } else {
                    chunk.data.clone()
                };
                output.extend_from_slice(&chunk.name);
                write_u32(output, if compressed { payload.len() as u32 } else { 0 });
                write_u32(output, chunk.data.len() as u32);
                write_u32(output, 0);
                output.extend_from_slice(&payload);
            });
            let mut capture = fixture(Scope::Model);
            capture.model_contents = bytes.clone();
            assert_eq!(assemble_capture(capture, Scope::Model).unwrap().0, bytes);
        }
    }
}
