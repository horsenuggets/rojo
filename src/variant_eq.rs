use std::cmp::Ordering;

use rbx_dom_weak::types::{PhysicalProperties, Variant, Vector3};

/// Replaces any NaN with one canonical NaN. Files store NaN without its sign or
/// payload, and Luau's `0/0` is a negative NaN on some platforms, so every NaN
/// must sort and hash the same or a value would never match its saved copy.
pub trait CanonicalNan: Copy {
    fn canonical_nan(self) -> Self;
}

impl CanonicalNan for f32 {
    fn canonical_nan(self) -> Self {
        if self.is_nan() {
            f32::NAN
        } else {
            self
        }
    }
}

impl CanonicalNan for f64 {
    fn canonical_nan(self) -> Self {
        if self.is_nan() {
            f64::NAN
        } else {
            self
        }
    }
}

/// Orders keypoint times, with every NaN equal and after every number.
pub fn compare_times(a: f32, b: f32) -> Ordering {
    a.canonical_nan().total_cmp(&b.canonical_nan())
}

/// Accepts three argumets: a float type and two values to compare.
///
/// Returns a bool indicating whether they're equal. This accounts for NaN such
/// that `approx_eq!(f32, f32::NAN, f32::NAN)` is `true`. Infinity only equals
/// itself, since `float_cmp` counts it as one ULP away from the largest finite
/// value.
macro_rules! approx_eq {
    ($Ty:ty, $a:expr, $b:expr) => {{
        let (a, b): ($Ty, $Ty) = ($a, $b);
        if a.is_finite() && b.is_finite() {
            float_cmp::approx_eq!($Ty, a, b)
        } else {
            a == b || a.is_nan() && b.is_nan()
        }
    }};
}

/// Compares two variants to determine if they're equal. This correctly takes
/// float comparisons into account.
pub fn variant_eq(variant_a: &Variant, variant_b: &Variant) -> bool {
    if variant_a.ty() != variant_b.ty() {
        return false;
    }

    match (variant_a, variant_b) {
        (Variant::Attributes(a), Variant::Attributes(b)) => {
            // If they're not the same size, we can just abort
            if a.len() != b.len() {
                return false;
            }

            // Since Attributes are stored with a BTreeMap, the keys are sorted
            // and we can compare each map's keys in order.
            for ((a_name, a_value), (b_name, b_value)) in a.iter().zip(b.iter()) {
                if !(a_name == b_name && variant_eq(a_value, b_value)) {
                    return false;
                }
            }

            true
        }
        (Variant::Axes(a), Variant::Axes(b)) => a == b,
        (Variant::BinaryString(a), Variant::BinaryString(b)) => a == b,
        (Variant::Bool(a), Variant::Bool(b)) => a == b,
        (Variant::BrickColor(a), Variant::BrickColor(b)) => a == b,
        (Variant::CFrame(a), Variant::CFrame(b)) => {
            vector_eq(&a.position, &b.position)
                && vector_eq(&a.orientation.x, &b.orientation.x)
                && vector_eq(&a.orientation.y, &b.orientation.y)
                && vector_eq(&a.orientation.z, &b.orientation.z)
        }
        (Variant::Color3(a), Variant::Color3(b)) => {
            approx_eq!(f32, a.r, b.r) && approx_eq!(f32, a.g, b.g) && approx_eq!(f32, a.b, b.b)
        }
        (Variant::Color3uint8(a), Variant::Color3uint8(b)) => a == b,
        (Variant::ColorSequence(a), Variant::ColorSequence(b)) => {
            if a.keypoints.len() != b.keypoints.len() {
                return false;
            }
            let mut a_keypoints: Vec<_> = a.keypoints.iter().collect();
            let mut b_keypoints: Vec<_> = b.keypoints.iter().collect();
            a_keypoints.sort_unstable_by(|k1, k2| compare_times(k1.time, k2.time));
            b_keypoints.sort_unstable_by(|k1, k2| compare_times(k1.time, k2.time));

            for (a_kp, b_kp) in a_keypoints.iter().zip(b_keypoints) {
                if !(approx_eq!(f32, a_kp.time, b_kp.time)
                    && approx_eq!(f32, a_kp.color.r, b_kp.color.r)
                    && approx_eq!(f32, a_kp.color.g, b_kp.color.g)
                    && approx_eq!(f32, a_kp.color.b, b_kp.color.b))
                {
                    return false;
                }
            }
            true
        }
        (Variant::Content(a), Variant::Content(b)) => a == b,
        (Variant::ContentId(a), Variant::ContentId(b)) => a == b,
        (Variant::Enum(a), Variant::Enum(b)) => a == b,
        (Variant::EnumItem(a), Variant::EnumItem(b)) => a == b,
        (Variant::Faces(a), Variant::Faces(b)) => a == b,
        (Variant::Float32(a), Variant::Float32(b)) => approx_eq!(f32, *a, *b),
        (Variant::Float64(a), Variant::Float64(b)) => approx_eq!(f64, *a, *b),
        (Variant::Font(a), Variant::Font(b)) => {
            a.weight == b.weight
                && a.style == b.style
                && a.family == b.family
                && a.cached_face_id == b.cached_face_id
        }
        (Variant::Int32(a), Variant::Int32(b)) => a == b,
        (Variant::Int64(a), Variant::Int64(b)) => a == b,
        (Variant::MaterialColors(a), Variant::MaterialColors(b)) => a.encode() == b.encode(),
        (Variant::NetAssetRef(a), Variant::NetAssetRef(b)) => a == b,
        (Variant::NumberRange(a), Variant::NumberRange(b)) => {
            approx_eq!(f32, a.max, b.max) && approx_eq!(f32, a.min, b.min)
        }
        (Variant::NumberSequence(a), Variant::NumberSequence(b)) => {
            if a.keypoints.len() != b.keypoints.len() {
                return false;
            }
            let mut a_keypoints: Vec<_> = a.keypoints.iter().collect();
            let mut b_keypoints: Vec<_> = b.keypoints.iter().collect();
            a_keypoints.sort_unstable_by(|k1, k2| compare_times(k1.time, k2.time));
            b_keypoints.sort_unstable_by(|k1, k2| compare_times(k1.time, k2.time));

            for (a_kp, b_kp) in a_keypoints.iter().zip(b_keypoints) {
                if !(approx_eq!(f32, a_kp.time, b_kp.time)
                    && approx_eq!(f32, a_kp.value, b_kp.value)
                    && approx_eq!(f32, a_kp.envelope, b_kp.envelope))
                {
                    return false;
                }
            }
            true
        }
        (Variant::OptionalCFrame(a), Variant::OptionalCFrame(b)) => match (a, b) {
            (Some(a), Some(b)) => {
                vector_eq(&a.position, &b.position)
                    && vector_eq(&a.orientation.x, &b.orientation.x)
                    && vector_eq(&a.orientation.y, &b.orientation.y)
                    && vector_eq(&a.orientation.z, &b.orientation.z)
            }
            (None, None) => true,
            _ => false,
        },
        (Variant::PhysicalProperties(a), Variant::PhysicalProperties(b)) => match (a, b) {
            (PhysicalProperties::Default, PhysicalProperties::Default) => true,
            (PhysicalProperties::Custom(a2), PhysicalProperties::Custom(b2)) => {
                approx_eq!(f32, a2.density(), b2.density())
                    && approx_eq!(f32, a2.elasticity(), b2.elasticity())
                    && approx_eq!(f32, a2.friction(), b2.friction())
                    && approx_eq!(f32, a2.elasticity_weight(), b2.elasticity_weight())
                    && approx_eq!(f32, a2.friction_weight(), b2.friction_weight())
                    && approx_eq!(f32, a2.acoustic_absorption(), b2.acoustic_absorption())
            }
            _ => false,
        },
        (Variant::Ray(a), Variant::Ray(b)) => {
            vector_eq(&a.direction, &b.direction) && vector_eq(&a.origin, &b.origin)
        }
        (Variant::Rect(a), Variant::Rect(b)) => {
            approx_eq!(f32, a.max.x, b.max.x)
                && approx_eq!(f32, a.max.y, b.max.y)
                && approx_eq!(f32, a.min.x, b.min.x)
                && approx_eq!(f32, a.min.y, b.min.y)
        }
        (Variant::Ref(a), Variant::Ref(b)) => a == b,
        (Variant::Region3(a), Variant::Region3(b)) => {
            vector_eq(&a.max, &b.max) && vector_eq(&a.min, &b.min)
        }
        (Variant::Region3int16(a), Variant::Region3int16(b)) => a == b,
        (Variant::SecurityCapabilities(a), Variant::SecurityCapabilities(b)) => a == b,
        (Variant::SharedString(a), Variant::SharedString(b)) => a == b,
        (Variant::Tags(a), Variant::Tags(b)) => {
            let mut a_sorted: Vec<&str> = a.iter().collect();
            let mut b_sorted: Vec<&str> = b.iter().collect();
            if a_sorted.len() == b_sorted.len() {
                a_sorted.sort_unstable();
                b_sorted.sort_unstable();
                for (a_tag, b_tag) in a_sorted.into_iter().zip(b_sorted) {
                    if a_tag != b_tag {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        }
        (Variant::UDim(a), Variant::UDim(b)) => {
            approx_eq!(f32, a.scale, b.scale) && a.offset == b.offset
        }
        (Variant::UDim2(a), Variant::UDim2(b)) => {
            approx_eq!(f32, a.x.scale, b.x.scale)
                && a.x.offset == b.x.offset
                && approx_eq!(f32, a.y.scale, b.y.scale)
                && a.y.offset == b.y.offset
        }
        (Variant::UniqueId(a), Variant::UniqueId(b)) => a == b,
        (Variant::String(a), Variant::String(b)) => a == b,
        (Variant::Vector2(a), Variant::Vector2(b)) => {
            approx_eq!(f32, a.x, b.x) && approx_eq!(f32, a.y, b.y)
        }
        (Variant::Vector2int16(a), Variant::Vector2int16(b)) => a == b,
        (Variant::Vector3(a), Variant::Vector3(b)) => vector_eq(a, b),
        (Variant::Vector3int16(a), Variant::Vector3int16(b)) => a == b,
        (a, b) => panic!(
            "unsupport variant comparison: {:?} and {:?}",
            a.ty(),
            b.ty()
        ),
    }
}

#[inline(always)]
fn vector_eq(a: &Vector3, b: &Vector3) -> bool {
    approx_eq!(f32, a.x, b.x) && approx_eq!(f32, a.y, b.y) && approx_eq!(f32, a.z, b.z)
}

#[cfg(test)]
mod test {
    use super::*;
    use rbx_dom_weak::types::{
        Color3, ColorSequence, ColorSequenceKeypoint, NumberSequence, NumberSequenceKeypoint,
    };

    /// Compares every pair of special values, which must only equal themselves.
    #[test]
    fn special_floats_only_equal_themselves() {
        let values = [
            0.0,
            1.0,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ];
        for (i, &a) in values.iter().enumerate() {
            for (j, &b) in values.iter().enumerate() {
                for (left, right) in [
                    (Variant::Float32(a), Variant::Float32(b)),
                    (Variant::Float64(a.into()), Variant::Float64(b.into())),
                    (
                        Vector3::new(0.0, a, 0.0).into(),
                        Vector3::new(0.0, b, 0.0).into(),
                    ),
                ] {
                    assert_eq!(variant_eq(&left, &right), i == j, "{left:?} and {right:?}");
                }
            }
        }
        assert!(variant_eq(
            &Variant::Float32(1.0),
            &Variant::Float32(1.0 + f32::EPSILON)
        ));
    }

    #[test]
    fn sequences_with_nan_times_compare_without_panicking() {
        let sequence = |time: f32| {
            Variant::NumberSequence(NumberSequence {
                keypoints: vec![
                    NumberSequenceKeypoint::new(time, 0.0, 0.0),
                    NumberSequenceKeypoint::new(1.0, 1.0, 0.0),
                ],
            })
        };
        assert!(variant_eq(&sequence(f32::NAN), &sequence(f32::NAN)));
        assert!(!variant_eq(&sequence(f32::NAN), &sequence(0.0)));
    }

    /// A NaN time read back from a file is always positive, so a sequence with
    /// a negative NaN time must still equal its saved copy, in both sequence
    /// types and whichever side the NaN keypoint starts on.
    #[test]
    fn sequences_match_their_saved_copy_whatever_the_nan_sign() {
        let negative_nan = -f32::NAN;
        assert!(negative_nan.is_sign_negative());
        for first in [true, false] {
            let order = |nan_time: f32| {
                let mut times = vec![nan_time, 0.5];
                if !first {
                    times.reverse();
                }
                times
            };
            let numbers = |nan_time: f32| {
                Variant::NumberSequence(NumberSequence {
                    keypoints: order(nan_time)
                        .into_iter()
                        .map(|time| NumberSequenceKeypoint::new(time, 1.0, 0.0))
                        .collect(),
                })
            };
            let colors = |nan_time: f32| {
                Variant::ColorSequence(ColorSequence {
                    keypoints: order(nan_time)
                        .into_iter()
                        .map(|time| ColorSequenceKeypoint::new(time, Color3::new(1.0, 0.0, 0.0)))
                        .collect(),
                })
            };
            for build in [&numbers as &dyn Fn(f32) -> Variant, &colors] {
                assert!(variant_eq(&build(negative_nan), &build(f32::NAN)));
            }
        }
    }
}
