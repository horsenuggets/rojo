use blake3::Hasher;
use rbx_dom_weak::types::{ContentType, PhysicalProperties, Variant, Vector3};

use crate::variant_eq::{compare_times, CanonicalNan};

// Scaling a huge finite value can overflow to infinity, which would hash it the same as a
// real infinity, so values that overflow are hashed unrounded instead. Every NaN hashes
// the same, since files don't keep its sign and arithmetic on it doesn't either.
macro_rules! round {
    ($exact:expr, $value:expr) => {{
        let value = $value.clone();
        if $exact {
            value.canonical_nan()
        } else {
            let scaled = value * 10.0;
            let rounded = if scaled.is_finite() || !value.is_finite() {
                scaled.round() / 10.0
            } else {
                value
            };
            rounded.canonical_nan()
        }
    }};
}

macro_rules! n_hash {
    ($hash:ident, $($num:expr),*) => {
        {$(
            $hash.update(&($num).to_le_bytes());
        )*}
    };
}

macro_rules! hash {
    ($hash:ident, $value:expr) => {{
        $hash.update($value);
    }};
}

pub fn hash_json_variant(hasher: &mut Hasher, value: &Variant) {
    // JSON rounds near-integers and loses negative zero, unlike binary exports.
    hash_variant_impl(
        hasher,
        &crate::resolution::normalize_json_variant(value.clone()),
        true,
    );
}

pub fn hash_exact_variant(hasher: &mut Hasher, value: &Variant) {
    hash_variant_impl(hasher, value, true);
}

/// Places `value` into the provided hasher.
pub fn hash_variant(hasher: &mut Hasher, value: &Variant) {
    hash_variant_impl(hasher, value, false);
}

fn hash_variant_impl(hasher: &mut Hasher, value: &Variant, exact: bool) {
    match value {
        Variant::Attributes(attrs) => {
            let mut sorted: Vec<(&String, &Variant)> = attrs.iter().collect();
            sorted.sort_unstable_by_key(|(name, _)| *name);
            for (name, attribute) in sorted {
                hasher.update(name.as_bytes());
                hash_variant_impl(hasher, attribute, exact);
            }
        }
        Variant::Axes(a) => hash!(hasher, &[a.bits()]),
        Variant::BinaryString(bytes) => hash!(hasher, bytes.as_ref()),
        Variant::Bool(bool) => hash!(hasher, &[*bool as u8]),
        Variant::BrickColor(color) => n_hash!(hasher, *color as u16),
        Variant::CFrame(cf) => {
            vector_hash(exact, hasher, cf.position);
            vector_hash(exact, hasher, cf.orientation.x);
            vector_hash(exact, hasher, cf.orientation.y);
            vector_hash(exact, hasher, cf.orientation.z);
        }
        Variant::Color3(color) => {
            n_hash!(
                hasher,
                round!(exact, color.r),
                round!(exact, color.g),
                round!(exact, color.b)
            )
        }
        Variant::Color3uint8(color) => hash!(hasher, &[color.r, color.b, color.g]),
        Variant::ColorSequence(seq) => {
            let mut new = Vec::with_capacity(seq.keypoints.len());
            for keypoint in &seq.keypoints {
                new.push(keypoint);
            }
            new.sort_unstable_by(|a, b| {
                compare_times(round!(exact, a.time), round!(exact, b.time))
            });
            for keypoint in new {
                n_hash!(
                    hasher,
                    round!(exact, keypoint.time),
                    round!(exact, keypoint.color.r),
                    round!(exact, keypoint.color.g),
                    round!(exact, keypoint.color.b)
                )
            }
        }
        Variant::Content(content) => match content.value() {
            ContentType::None => {
                hash!(hasher, &[0]);
            }
            ContentType::Uri(uri) => {
                hash!(hasher, &[1]);
                hash!(hasher, uri.as_bytes());
            }
            ContentType::Object(referent) => {
                hash!(hasher, &[2]);
                hash!(hasher, referent.to_string().as_bytes())
            }
            other => {
                panic!("the ContentType {other:?} cannot be hashed as a Variant")
            }
        },
        Variant::ContentId(content) => {
            let s: &str = content.as_ref();
            hash!(hasher, s.as_bytes())
        }
        Variant::Enum(e) => n_hash!(hasher, e.to_u32()),
        Variant::EnumItem(e) => {
            hash!(hasher, e.ty.as_bytes());
            n_hash!(hasher, e.value);
        }
        Variant::Faces(f) => hash!(hasher, &[f.bits()]),
        Variant::Float32(n) => n_hash!(hasher, round!(exact, *n)),
        Variant::Float64(n) => n_hash!(hasher, round!(exact, n)),
        Variant::Font(f) => {
            n_hash!(hasher, f.weight as u16);
            n_hash!(hasher, f.style as u8);
            hash!(hasher, f.family.as_bytes());
            if let Some(cache) = &f.cached_face_id {
                hash!(hasher, &[0x01]);
                hash!(hasher, cache.as_bytes());
            } else {
                hash!(hasher, &[0x00]);
            }
        }
        Variant::Int32(n) => n_hash!(hasher, n),
        Variant::Int64(n) => n_hash!(hasher, n),
        Variant::MaterialColors(n) => hash!(hasher, n.encode().as_slice()),
        Variant::NetAssetRef(net_asset) => hash!(hasher, net_asset.hash().as_bytes()),
        Variant::NumberRange(nr) => n_hash!(hasher, round!(exact, nr.max), round!(exact, nr.min)),
        Variant::NumberSequence(seq) => {
            let mut new = Vec::with_capacity(seq.keypoints.len());
            for keypoint in &seq.keypoints {
                new.push(keypoint);
            }
            new.sort_unstable_by(|a, b| {
                compare_times(round!(exact, a.time), round!(exact, b.time))
            });
            for keypoint in new {
                n_hash!(
                    hasher,
                    round!(exact, keypoint.time),
                    round!(exact, keypoint.value),
                    round!(exact, keypoint.envelope)
                )
            }
        }
        Variant::OptionalCFrame(maybe_cf) => {
            if let Some(cf) = maybe_cf {
                hash!(hasher, &[0x01]);
                vector_hash(exact, hasher, cf.position);
                vector_hash(exact, hasher, cf.orientation.x);
                vector_hash(exact, hasher, cf.orientation.y);
                vector_hash(exact, hasher, cf.orientation.z);
            } else {
                hash!(hasher, &[0x00]);
            }
        }
        Variant::PhysicalProperties(properties) => match properties {
            PhysicalProperties::Default => hash!(hasher, &[0x00]),
            PhysicalProperties::Custom(custom) => {
                hash!(hasher, &[0x00]);
                n_hash!(
                    hasher,
                    round!(exact, custom.density()),
                    round!(exact, custom.friction()),
                    round!(exact, custom.elasticity()),
                    round!(exact, custom.friction_weight()),
                    round!(exact, custom.elasticity_weight()),
                    round!(exact, custom.acoustic_absorption())
                )
            }
        },
        Variant::Ray(ray) => {
            vector_hash(exact, hasher, ray.origin);
            vector_hash(exact, hasher, ray.direction);
        }
        Variant::Rect(rect) => n_hash!(
            hasher,
            round!(exact, rect.max.x),
            round!(exact, rect.max.y),
            round!(exact, rect.min.x),
            round!(exact, rect.min.y)
        ),
        Variant::Ref(referent) => hash!(hasher, referent.to_string().as_bytes()),
        Variant::Region3(region) => {
            vector_hash(exact, hasher, region.max);
            vector_hash(exact, hasher, region.min);
        }
        Variant::Region3int16(region) => {
            n_hash!(
                hasher,
                region.max.x,
                region.max.y,
                region.max.z,
                region.min.x,
                region.min.y,
                region.min.z
            )
        }
        Variant::SecurityCapabilities(capabilities) => n_hash!(hasher, capabilities.bits()),
        Variant::SharedString(sstr) => hash!(hasher, sstr.hash().as_bytes()),
        Variant::String(str) => hash!(hasher, str.as_bytes()),
        Variant::Tags(tags) => {
            let mut dupe: Vec<&str> = tags.iter().collect();
            dupe.sort_unstable();
            for tag in dupe {
                hash!(hasher, tag.as_bytes())
            }
        }
        Variant::UDim(udim) => n_hash!(hasher, round!(exact, udim.scale), udim.offset),
        Variant::UDim2(udim) => n_hash!(
            hasher,
            round!(exact, udim.y.scale),
            udim.y.offset,
            round!(exact, udim.x.scale),
            udim.x.offset
        ),
        Variant::Vector2(v2) => n_hash!(hasher, round!(exact, v2.x), round!(exact, v2.y)),
        Variant::Vector2int16(v2) => n_hash!(hasher, v2.x, v2.y),
        Variant::Vector3(v3) => vector_hash(exact, hasher, *v3),
        Variant::Vector3int16(v3) => n_hash!(hasher, v3.x, v3.y, v3.z),

        // Hashing UniqueId properties doesn't make sense
        Variant::UniqueId(_) => (),

        unknown => {
            log::warn!(
                "Encountered unknown Variant {:?} while hashing",
                unknown.ty()
            )
        }
    }
}

fn vector_hash(exact: bool, hasher: &mut Hasher, vector: Vector3) {
    n_hash!(
        hasher,
        round!(exact, vector.x),
        round!(exact, vector.y),
        round!(exact, vector.z)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::types::{Attributes, NumberRange, NumberSequence, NumberSequenceKeypoint};

    /// Non-finite composite values are written to metadata too, so each one
    /// must hash differently from the others and from finite values, and the
    /// same value must always hash the same.
    #[test]
    fn non_finite_composite_attributes_are_hashed() {
        let hash = |value: Variant| {
            let mut attributes = Attributes::new();
            attributes.insert("Value".to_owned(), value);
            let mut hasher = Hasher::new();
            hash_variant(&mut hasher, &Variant::Attributes(attributes));
            hasher.finalize()
        };
        let sequence = |time: f32| {
            Variant::NumberSequence(NumberSequence {
                keypoints: vec![
                    NumberSequenceKeypoint::new(time, 0.0, 0.0),
                    NumberSequenceKeypoint::new(1.0, 1.0, 0.0),
                ],
            })
        };
        let values = [
            Vector3::new(0.0, 0.0, 0.0).into(),
            Vector3::new(f32::NAN, 0.0, 0.0).into(),
            Vector3::new(f32::INFINITY, 0.0, 0.0).into(),
            Vector3::new(f32::NEG_INFINITY, 0.0, 0.0).into(),
            Vector3::new(f32::MAX, 0.0, 0.0).into(),
            NumberRange::new(0.0, 1.0).into(),
            NumberRange::new(0.0, f32::INFINITY).into(),
            sequence(0.0),
            sequence(f32::NAN),
        ];
        let hashes: Vec<_> = values.iter().cloned().map(hash).collect();
        for (index, a) in hashes.iter().enumerate() {
            for b in &hashes[index + 1..] {
                assert_ne!(a, b);
            }
        }
        for value in values {
            assert_eq!(hash(value.clone()), hash(value));
        }

        // Files read NaN back as positive, so its sign must not change a hash.
        let negative_nan = -f32::NAN;
        assert_eq!(
            hash(Vector3::new(negative_nan, 0.0, 0.0).into()),
            hash(Vector3::new(f32::NAN, 0.0, 0.0).into())
        );
        assert_eq!(hash(sequence(negative_nan)), hash(sequence(f32::NAN)));
    }

    #[test]
    fn rounded_zero_has_one_json_hash_regardless_of_sign() {
        let hash = |value: Variant| {
            let mut attributes = Attributes::new();
            attributes.insert("Value".to_owned(), value);
            let mut hasher = Hasher::new();
            hash_json_variant(&mut hasher, &Variant::Attributes(attributes));
            hasher.finalize()
        };
        for value in [-0.0, -0.0000005, 0.0, 0.0000005] {
            assert_eq!(hash(Variant::Float64(value)), hash(Variant::Float64(0.0)));
            assert_eq!(
                hash(Variant::Float32(value as f32)),
                hash(Variant::Float32(0.0))
            );
            assert_eq!(
                hash(Vector3::new(value as f32, 0.0, 0.0).into()),
                hash(Vector3::new(0.0, 0.0, 0.0).into())
            );
        }
    }

    #[test]
    fn json_hashes_keep_fractional_changes_in_attributes_and_composites() {
        let values = |value: f32| {
            let mut nested = Attributes::new();
            nested.insert("Nested".into(), Variant::Float64(value as f64));
            vec![
                Variant::Float32(value),
                Variant::Float64(value as f64),
                Variant::Vector3(Vector3::new(value, 0.0, 0.0)),
                Variant::NumberRange(NumberRange::new(value, 1.0)),
                Variant::NumberSequence(NumberSequence {
                    keypoints: vec![NumberSequenceKeypoint::new(0.0, value, 0.0)],
                }),
                Variant::Attributes(nested),
            ]
        };
        for (before, after) in values(-0.0000005).into_iter().zip(values(0.01)) {
            let hash = |value: Variant| {
                let mut attributes = Attributes::new();
                attributes.insert("Value".into(), value);
                let mut hasher = Hasher::new();
                hash_json_variant(&mut hasher, &Variant::Attributes(attributes));
                hasher.finalize()
            };
            assert_ne!(hash(before), hash(after));
        }
    }

    /// Non-finite numbers are written to metadata, so changing one must
    /// change the hash or syncback would never write the new value.
    #[test]
    fn non_finite_number_attributes_are_hashed() {
        let hash = |value: f64| {
            let mut attributes = Attributes::new();
            attributes.insert("EmitDuration".to_owned(), value.into());
            let mut hasher = Hasher::new();
            hash_variant(&mut hasher, &Variant::Attributes(attributes));
            hasher.finalize()
        };
        let hashes = [
            hash(1.0),
            hash(f64::MAX),
            hash(f64::MIN),
            hash(1e308),
            hash(-1e308),
            hash(f64::INFINITY),
            hash(f64::NEG_INFINITY),
            hash(f64::NAN),
        ];
        for (index, a) in hashes.iter().enumerate() {
            for b in &hashes[index + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(hash(f64::NAN), hash(f64::NAN));
    }
}
