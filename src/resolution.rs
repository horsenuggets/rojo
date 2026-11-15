use std::borrow::Borrow;

use anyhow::{bail, format_err};
use rbx_dom_weak::types::{
    Attributes, CFrame, Color3, ColorSequence, ColorSequenceKeypoint, Content, ContentId,
    ContentType, Enum, Font, MaterialColors, Matrix3, NumberRange, NumberSequence,
    NumberSequenceKeypoint, Ray, Rect, Region3, Tags, UDim, UDim2, Variant, VariantType, Vector2,
    Vector3,
};
use rbx_reflection::{DataType, PropertyDescriptor};
use serde::{Deserialize, Serialize};

use crate::REF_POINTER_ATTRIBUTE_PREFIX;

/// A user-friendly version of `Variant` that supports specifying ambiguous
/// values. Ambiguous values need a reflection database to be resolved to a
/// usable value.
///
/// This type is used in Rojo projects and JSON models to make specifying the
/// most common types of properties, like strings or vectors, much easier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnresolvedValue {
    FullyQualified(Variant),
    NonFinite(NonFiniteValue),
    Ambiguous(AmbiguousValue),
}

/// A value JSON can't hold because it is or contains NaN or infinity. It's
/// written in the same shape as its fully qualified form, with each non-finite
/// number spelled as a string, such as `{"Float64": "inf"}` or
/// `{"NumberRange": [0.0, "inf"]}`. Roblox often uses `math.huge` to mean
/// "forever" or "unbounded", like the default `UISizeConstraint.MaxSize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NonFiniteValue {
    Float32(NonFinite),
    Float64(NonFinite),
    Vector2([Float; 2]),
    Vector3([Float; 3]),
    Color3([Float; 3]),
    CFrame(TaggedCFrame),
    OptionalCFrame(TaggedCFrame),
    NumberRange([Float; 2]),
    NumberSequence(TaggedSequence<TaggedNumberKeypoint>),
    ColorSequence(TaggedSequence<TaggedColorKeypoint>),
    Ray(TaggedRay),
    Rect([[Float; 2]; 2]),
    Region3([[Float; 3]; 2]),
    UDim((Float, i32)),
    UDim2([(Float, i32); 2]),
}

/// One component of a composite `NonFiniteValue`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Float {
    Finite(f32),
    NonFinite(NonFinite),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedCFrame {
    position: [Float; 3],
    orientation: [[Float; 3]; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedRay {
    origin: [Float; 3],
    direction: [Float; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedSequence<K> {
    keypoints: Vec<K>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedNumberKeypoint {
    time: Float,
    value: Float,
    envelope: Float,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedColorKeypoint {
    time: Float,
    color: [Float; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NonFinite {
    #[serde(rename = "inf")]
    Infinity,
    #[serde(rename = "-inf")]
    NegativeInfinity,
    #[serde(rename = "nan")]
    NaN,
}

impl NonFinite {
    fn new(value: f64) -> Option<Self> {
        if value.is_nan() {
            Some(Self::NaN)
        } else if value == f64::INFINITY {
            Some(Self::Infinity)
        } else if value == f64::NEG_INFINITY {
            Some(Self::NegativeInfinity)
        } else {
            None
        }
    }

    fn value(self) -> f64 {
        match self {
            Self::Infinity => f64::INFINITY,
            Self::NegativeInfinity => f64::NEG_INFINITY,
            Self::NaN => f64::NAN,
        }
    }
}

impl Float {
    fn value(self) -> f32 {
        match self {
            Self::Finite(value) => value,
            Self::NonFinite(value) => value.value() as f32,
        }
    }
}

/// Builds the tagged form of a value, noting whether any component needed it.
#[derive(Default)]
struct Encoder {
    found_non_finite: bool,
}

impl Encoder {
    fn float(&mut self, value: f32) -> Float {
        match NonFinite::new(value.into()) {
            Some(non_finite) => {
                self.found_non_finite = true;
                Float::NonFinite(non_finite)
            }
            None => Float::Finite(value),
        }
    }

    fn vector2(&mut self, vector: Vector2) -> [Float; 2] {
        [self.float(vector.x), self.float(vector.y)]
    }

    fn vector3(&mut self, vector: Vector3) -> [Float; 3] {
        [
            self.float(vector.x),
            self.float(vector.y),
            self.float(vector.z),
        ]
    }

    fn cframe(&mut self, cframe: &CFrame) -> TaggedCFrame {
        TaggedCFrame {
            position: self.vector3(cframe.position),
            orientation: [
                self.vector3(cframe.orientation.x),
                self.vector3(cframe.orientation.y),
                self.vector3(cframe.orientation.z),
            ],
        }
    }

    fn udim(&mut self, udim: UDim) -> (Float, i32) {
        (self.float(udim.scale), udim.offset)
    }

    /// Returns the tagged form of `variant` whether or not it holds a
    /// non-finite number, or `None` for types that have no tagged form.
    fn encode(&mut self, variant: &Variant) -> Option<NonFiniteValue> {
        Some(match variant {
            Variant::Float32(value) => {
                let value = NonFinite::new((*value).into())?;
                self.found_non_finite = true;
                NonFiniteValue::Float32(value)
            }
            Variant::Float64(value) => {
                let value = NonFinite::new(*value)?;
                self.found_non_finite = true;
                NonFiniteValue::Float64(value)
            }
            Variant::Vector2(vector) => NonFiniteValue::Vector2(self.vector2(*vector)),
            Variant::Vector3(vector) => NonFiniteValue::Vector3(self.vector3(*vector)),
            Variant::Color3(color) => NonFiniteValue::Color3([
                self.float(color.r),
                self.float(color.g),
                self.float(color.b),
            ]),
            Variant::CFrame(cframe) => NonFiniteValue::CFrame(self.cframe(cframe)),
            Variant::OptionalCFrame(Some(cframe)) => {
                NonFiniteValue::OptionalCFrame(self.cframe(cframe))
            }
            Variant::NumberRange(range) => {
                NonFiniteValue::NumberRange([self.float(range.min), self.float(range.max)])
            }
            Variant::NumberSequence(sequence) => NonFiniteValue::NumberSequence(TaggedSequence {
                keypoints: sequence
                    .keypoints
                    .iter()
                    .map(|point| TaggedNumberKeypoint {
                        time: self.float(point.time),
                        value: self.float(point.value),
                        envelope: self.float(point.envelope),
                    })
                    .collect(),
            }),
            Variant::ColorSequence(sequence) => NonFiniteValue::ColorSequence(TaggedSequence {
                keypoints: sequence
                    .keypoints
                    .iter()
                    .map(|point| TaggedColorKeypoint {
                        time: self.float(point.time),
                        color: [
                            self.float(point.color.r),
                            self.float(point.color.g),
                            self.float(point.color.b),
                        ],
                    })
                    .collect(),
            }),
            Variant::Ray(ray) => NonFiniteValue::Ray(TaggedRay {
                origin: self.vector3(ray.origin),
                direction: self.vector3(ray.direction),
            }),
            Variant::Rect(rect) => {
                NonFiniteValue::Rect([self.vector2(rect.min), self.vector2(rect.max)])
            }
            Variant::Region3(region) => {
                NonFiniteValue::Region3([self.vector3(region.min), self.vector3(region.max)])
            }
            Variant::UDim(udim) => NonFiniteValue::UDim(self.udim(*udim)),
            Variant::UDim2(udim) => NonFiniteValue::UDim2([self.udim(udim.x), self.udim(udim.y)]),
            _ => return None,
        })
    }
}

fn vector2([x, y]: [Float; 2]) -> Vector2 {
    Vector2::new(x.value(), y.value())
}

fn vector3([x, y, z]: [Float; 3]) -> Vector3 {
    Vector3::new(x.value(), y.value(), z.value())
}

fn cframe(cframe: TaggedCFrame) -> CFrame {
    let [x, y, z] = cframe.orientation;
    CFrame::new(
        vector3(cframe.position),
        Matrix3::new(vector3(x), vector3(y), vector3(z)),
    )
}

fn udim((scale, offset): (Float, i32)) -> UDim {
    UDim::new(scale.value(), offset)
}

impl NonFiniteValue {
    /// Returns the tagged form of `variant`, if it is or contains a non-finite
    /// number.
    fn from_variant(variant: &Variant) -> Option<Self> {
        let mut encoder = Encoder::default();
        let value = encoder.encode(variant)?;
        encoder.found_non_finite.then_some(value)
    }

    fn into_variant(self) -> Variant {
        match self {
            Self::Float32(value) => Variant::Float32(value.value() as f32),
            Self::Float64(value) => Variant::Float64(value.value()),
            Self::Vector2(value) => vector2(value).into(),
            Self::Vector3(value) => vector3(value).into(),
            Self::Color3([r, g, b]) => Color3::new(r.value(), g.value(), b.value()).into(),
            Self::CFrame(value) => cframe(value).into(),
            Self::OptionalCFrame(value) => Variant::OptionalCFrame(Some(cframe(value))),
            Self::NumberRange([min, max]) => NumberRange::new(min.value(), max.value()).into(),
            Self::NumberSequence(sequence) => NumberSequence {
                keypoints: sequence
                    .keypoints
                    .into_iter()
                    .map(|point| {
                        NumberSequenceKeypoint::new(
                            point.time.value(),
                            point.value.value(),
                            point.envelope.value(),
                        )
                    })
                    .collect(),
            }
            .into(),
            Self::ColorSequence(sequence) => ColorSequence {
                keypoints: sequence
                    .keypoints
                    .into_iter()
                    .map(|point| {
                        let [r, g, b] = point.color;
                        ColorSequenceKeypoint::new(
                            point.time.value(),
                            Color3::new(r.value(), g.value(), b.value()),
                        )
                    })
                    .collect(),
            }
            .into(),
            Self::Ray(ray) => Ray::new(vector3(ray.origin), vector3(ray.direction)).into(),
            Self::Rect([min, max]) => Rect::new(vector2(min), vector2(max)).into(),
            Self::Region3([min, max]) => Region3::new(vector3(min), vector3(max)).into(),
            Self::UDim(value) => udim(value).into(),
            Self::UDim2([x, y]) => UDim2::new(udim(x), udim(y)).into(),
        }
    }
}

impl UnresolvedValue {
    pub fn resolve(self, class_name: &str, prop_name: &str) -> anyhow::Result<Variant> {
        match self {
            UnresolvedValue::FullyQualified(full) => Ok(full),
            UnresolvedValue::NonFinite(value) => Ok(value.into_variant()),
            UnresolvedValue::Ambiguous(partial) => partial.resolve(class_name, prop_name),
        }
    }

    pub fn resolve_unambiguous(self) -> anyhow::Result<Variant> {
        match self {
            UnresolvedValue::FullyQualified(full) => Ok(full),
            UnresolvedValue::NonFinite(value) => Ok(value.into_variant()),
            UnresolvedValue::Ambiguous(partial) => partial.resolve_unambiguous(),
        }
    }

    /// Creates an `UnresolvedValue` from a variant, using a class and property
    /// name to potentially allow for ambiguous Enum variants.
    pub fn from_variant(variant: Variant, class_name: &str, prop_name: &str) -> Self {
        // Checked first, since the ambiguous forms would write NaN and infinity
        // as `null`.
        if let Some(value) = NonFiniteValue::from_variant(&variant) {
            return Self::NonFinite(value);
        }
        let descriptor = find_descriptor(class_name, prop_name);
        if descriptor.is_some() {
            // We can only use an ambiguous syntax if the property is known
            // to the reflection database.
            Self::Ambiguous(match variant {
                Variant::Enum(rbx_enum) => {
                    if let Some(property) = descriptor {
                        if let DataType::Enum(enum_name) = &property.data_type {
                            let database = rbx_reflection_database::get().unwrap();
                            if let Some(enum_descriptor) = database.enums.get(enum_name) {
                                for (variant_name, id) in &enum_descriptor.items {
                                    if *id == rbx_enum.to_u32() {
                                        return Self::Ambiguous(AmbiguousValue::String(
                                            variant_name.to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    return Self::FullyQualified(variant);
                }
                Variant::Bool(bool) => AmbiguousValue::Bool(bool),
                Variant::Float32(n) => AmbiguousValue::Number(widen(n)),
                Variant::Float64(n) => AmbiguousValue::Number(n),
                Variant::Int32(n) => AmbiguousValue::Number(n as f64),
                Variant::Int64(n) => AmbiguousValue::Number(n as f64),
                Variant::String(str) => AmbiguousValue::String(str),
                Variant::Tags(tags) => {
                    AmbiguousValue::StringArray(tags.iter().map(|s| s.to_string()).collect())
                }
                Variant::Content(ref content) => match content.value() {
                    ContentType::None => AmbiguousValue::String(String::new()),
                    ContentType::Uri(uri) => AmbiguousValue::String(uri.clone()),
                    _ => return Self::FullyQualified(variant),
                },
                Variant::ContentId(content) => AmbiguousValue::String(content.into_string()),
                Variant::Vector2(vector) => {
                    AmbiguousValue::Array2([widen(vector.x), widen(vector.y)])
                }
                Variant::Vector3(vector) => {
                    AmbiguousValue::Array3([widen(vector.x), widen(vector.y), widen(vector.z)])
                }
                Variant::Color3(color) => {
                    AmbiguousValue::Array3([widen(color.r), widen(color.g), widen(color.b)])
                }
                Variant::CFrame(cf) => AmbiguousValue::Array12([
                    widen(cf.position.x),
                    widen(cf.position.y),
                    widen(cf.position.z),
                    widen(cf.orientation.x.x),
                    widen(cf.orientation.x.y),
                    widen(cf.orientation.x.z),
                    widen(cf.orientation.y.x),
                    widen(cf.orientation.y.y),
                    widen(cf.orientation.y.z),
                    widen(cf.orientation.z.x),
                    widen(cf.orientation.z.y),
                    widen(cf.orientation.z.z),
                ]),
                Variant::Attributes(attr) => AmbiguousValue::Attributes(attr),
                Variant::Font(font) => AmbiguousValue::Font(font),
                Variant::MaterialColors(colors) => AmbiguousValue::MaterialColors(colors),
                _ => {
                    return Self::FullyQualified(variant);
                }
            })
        } else {
            Self::FullyQualified(variant)
        }
    }

    /// Creates an `UnresolvedValue` from a variant, only returning ambiguous
    /// values if they're able to be resolved in a context-free environment.
    pub fn from_variant_unambiguous(variant: Variant) -> Self {
        if let Some(value) = NonFiniteValue::from_variant(&variant) {
            return Self::NonFinite(value);
        }
        match variant {
            Variant::String(str) => Self::Ambiguous(AmbiguousValue::String(str)),
            Variant::Float64(number) => Self::Ambiguous(AmbiguousValue::Number(number)),
            Variant::Bool(bool) => Self::Ambiguous(AmbiguousValue::Bool(bool)),
            Variant::BinaryString(bstr) => match std::str::from_utf8(bstr.as_ref()) {
                Ok(_) => Self::Ambiguous(AmbiguousValue::String(
                    String::from_utf8(bstr.into_vec()).unwrap(),
                )),
                Err(_) => Self::FullyQualified(Variant::BinaryString(bstr)),
            },
            _ => Self::FullyQualified(variant),
        }
    }
}

/// Widens an `f32` through its shortest decimal form, so `0.6` is written as
/// `0.6` instead of `0.6000000238418579`. Narrowing the result back to `f32`
/// always gives the original value.
fn widen(value: f32) -> f64 {
    value.to_string().parse().unwrap_or(value as f64)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AmbiguousValue {
    Bool(bool),
    String(String),
    StringArray(Vec<String>),
    Number(f64),
    Array2([f64; 2]),
    Array3([f64; 3]),
    Array4([f64; 4]),
    Array12([f64; 12]),
    Attributes(Attributes),
    Font(Font),
    MaterialColors(MaterialColors),
}

impl AmbiguousValue {
    pub fn resolve(self, class_name: &str, prop_name: &str) -> anyhow::Result<Variant> {
        let property = find_descriptor(class_name, prop_name)
            .ok_or_else(|| format_err!("Unknown property {}.{}", class_name, prop_name))?;

        match &property.data_type {
            DataType::Enum(enum_name) => {
                let database = rbx_reflection_database::get().unwrap();

                let enum_descriptor = database.enums.get(enum_name).ok_or_else(|| {
                    format_err!("Unknown enum {}. This is a Rojo bug!", enum_name)
                })?;

                let error = |what: &str| {
                    let mut all_values = enum_descriptor
                        .items
                        .keys()
                        .map(|value| value.borrow())
                        .collect::<Vec<_>>();
                    all_values.sort();

                    let examples = nonexhaustive_list(&all_values);

                    format_err!(
                        "Invalid value for property {}.{}. Got {} but \
                         expected a member of the {} enum such as {}",
                        class_name,
                        prop_name,
                        what,
                        enum_name,
                        examples,
                    )
                };

                let value = match self {
                    AmbiguousValue::String(value) => value,
                    unresolved => return Err(error(unresolved.describe())),
                };

                let resolved = enum_descriptor
                    .items
                    .get(value.as_str())
                    .ok_or_else(|| error(value.as_str()))?;

                Ok(Enum::from_u32(*resolved).into())
            }
            DataType::Value(variant_ty) => match (variant_ty, self) {
                (VariantType::Bool, AmbiguousValue::Bool(value)) => Ok(value.into()),

                (VariantType::Float32, AmbiguousValue::Number(value)) => Ok((value as f32).into()),
                (VariantType::Float64, AmbiguousValue::Number(value)) => Ok(value.into()),
                (VariantType::Int32, AmbiguousValue::Number(value)) => Ok((value as i32).into()),
                (VariantType::Int64, AmbiguousValue::Number(value)) => Ok((value as i64).into()),

                (VariantType::String, AmbiguousValue::String(value)) => Ok(value.into()),
                (VariantType::Tags, AmbiguousValue::StringArray(value)) => {
                    Ok(Tags::from(value).into())
                }
                (VariantType::Content, AmbiguousValue::String(value)) => {
                    Ok(Content::from(value).into())
                }
                (VariantType::ContentId, AmbiguousValue::String(value)) => {
                    Ok(ContentId::from(value).into())
                }

                (VariantType::Vector2, AmbiguousValue::Array2(value)) => {
                    Ok(Vector2::new(value[0] as f32, value[1] as f32).into())
                }

                (VariantType::Vector3, AmbiguousValue::Array3(value)) => {
                    Ok(Vector3::new(value[0] as f32, value[1] as f32, value[2] as f32).into())
                }

                (VariantType::Color3, AmbiguousValue::Array3(value)) => {
                    Ok(Color3::new(value[0] as f32, value[1] as f32, value[2] as f32).into())
                }

                (VariantType::CFrame, AmbiguousValue::Array12(value)) => {
                    let value = value.map(|v| v as f32);
                    let pos = Vector3::new(value[0], value[1], value[2]);
                    let orientation = Matrix3::new(
                        Vector3::new(value[3], value[4], value[5]),
                        Vector3::new(value[6], value[7], value[8]),
                        Vector3::new(value[9], value[10], value[11]),
                    );

                    Ok(CFrame::new(pos, orientation).into())
                }

                (VariantType::Attributes, AmbiguousValue::Attributes(value)) => Ok(value.into()),

                (VariantType::Font, AmbiguousValue::Font(value)) => Ok(value.into()),

                (VariantType::MaterialColors, AmbiguousValue::MaterialColors(value)) => {
                    Ok(value.into())
                }

                (VariantType::Ref, AmbiguousValue::String(_)) => Err(format_err!(
                    "Cannot resolve Ref properties as a String.\
                    Use an attribute named `{REF_POINTER_ATTRIBUTE_PREFIX}{prop_name}"
                )),
                (_, unresolved) => Err(format_err!(
                    "Wrong type of value for property {}.{}. Expected {:?}, got {}",
                    class_name,
                    prop_name,
                    variant_ty,
                    unresolved.describe(),
                )),
            },
            _ => Err(format_err!(
                "Unknown data type for property {}.{}",
                class_name,
                prop_name
            )),
        }
    }

    pub fn resolve_unambiguous(self) -> anyhow::Result<Variant> {
        match self {
            AmbiguousValue::Bool(value) => Ok(value.into()),
            AmbiguousValue::Number(value) => Ok(value.into()),
            AmbiguousValue::String(value) => Ok(value.into()),

            other => bail!("Cannot unambiguously resolve the value {other:?}"),
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            AmbiguousValue::Bool(_) => "a bool",
            AmbiguousValue::String(_) => "a string",
            AmbiguousValue::StringArray(_) => "an array of strings",
            AmbiguousValue::Number(_) => "a number",
            AmbiguousValue::Array2(_) => "an array of two numbers",
            AmbiguousValue::Array3(_) => "an array of three numbers",
            AmbiguousValue::Array4(_) => "an array of four numbers",
            AmbiguousValue::Array12(_) => "an array of twelve numbers",
            AmbiguousValue::Attributes(_) => "an object containing attributes",
            AmbiguousValue::Font(_) => "an object describing a Font",
            AmbiguousValue::MaterialColors(_) => "an object describing MaterialColors",
        }
    }
}

fn find_descriptor(
    class_name: &str,
    prop_name: &str,
) -> Option<&'static PropertyDescriptor<'static>> {
    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = class_name;

    loop {
        let class = database.classes.get(current_class_name)?;
        if let Some(descriptor) = class.properties.get(prop_name) {
            return Some(descriptor);
        }

        current_class_name = class.superclass?;
    }
}

/// Outputs a string containing up to MAX_ITEMS entries from the given list. If
/// there are more than MAX_ITEMS items, the number of remaining items will be
/// listed.
fn nonexhaustive_list(values: &[&str]) -> String {
    use std::fmt::Write;

    const MAX_ITEMS: usize = 8;

    let mut output = String::new();

    let last_index = values.len() - 1;
    let main_length = last_index.min(9);

    let main_list = &values[..main_length];
    for value in main_list {
        output.push_str(value);
        output.push_str(", ");
    }

    if values.len() > MAX_ITEMS {
        write!(output, "or {} more", values.len() - main_length).unwrap();
    } else {
        output.push_str("or ");
        output.push_str(values[values.len() - 1]);
    }

    output
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{json, variant_eq::variant_eq};

    fn resolve(class: &str, prop: &str, json_value: &str) -> Variant {
        let unresolved: UnresolvedValue = json::from_str(json_value).unwrap();
        unresolved.resolve(class, prop).unwrap()
    }

    fn resolve_unambiguous(json_value: &str) -> Variant {
        let unresolved: UnresolvedValue = json::from_str(json_value).unwrap();
        unresolved.resolve_unambiguous().unwrap()
    }

    #[test]
    fn float32_values_are_written_in_their_shortest_form() {
        let unresolved =
            UnresolvedValue::from_variant(Variant::Float32(0.6), "Part", "Transparency");
        assert_eq!(json::to_string_pretty(&unresolved).unwrap().trim(), "0.6");

        let size = UnresolvedValue::from_variant(
            Variant::Vector3(Vector3::new(179.694, 0.1, 2.0)),
            "Part",
            "Size",
        );
        assert_eq!(
            json::to_string_pretty(&size)
                .unwrap()
                .split_whitespace()
                .collect::<String>(),
            "[179.694,0.1,2.0]"
        );
    }

    /// Shortening must never change the value. Checks values across the whole
    /// `f32` range, including subnormals and both signs, survive a trip through
    /// JSON text and back to the exact same bits.
    #[test]
    fn float32_values_round_trip_exactly() {
        let mut bits: u32 = 1;
        for _ in 0..200_000 {
            bits = bits.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let value = f32::from_bits(bits);
            if !value.is_finite() {
                continue;
            }
            let text = json::to_string_pretty(&UnresolvedValue::from_variant(
                Variant::Float32(value),
                "Part",
                "Transparency",
            ))
            .unwrap();
            assert_eq!(
                resolve("Part", "Transparency", &text),
                Variant::Float32(value),
                "{value:e} was written as {text}"
            );
        }
    }

    /// Walks every non-finite value in both float types through property and
    /// attribute writes, checking the text and that it reads back exactly.
    #[test]
    fn non_finite_numbers_round_trip_through_tagged_text() {
        let cases = [
            (f64::INFINITY, "\"inf\""),
            (f64::NEG_INFINITY, "\"-inf\""),
            (f64::NAN, "\"nan\""),
        ];
        let same = |a: &Variant, b: &Variant| match (a, b) {
            (Variant::Float32(a), Variant::Float32(b)) => a.to_bits() == b.to_bits(),
            (Variant::Float64(a), Variant::Float64(b)) => a.to_bits() == b.to_bits(),
            _ => false,
        };
        for (value, text) in cases {
            for (variant, tag, class, prop) in [
                (
                    Variant::Float32(value as f32),
                    "Float32",
                    "Part",
                    "Transparency",
                ),
                (Variant::Float64(value), "Float64", "NumberValue", "Value"),
            ] {
                let expected = format!("{{\"{tag}\":{text}}}");
                for unresolved in [
                    UnresolvedValue::from_variant(variant.clone(), class, prop),
                    UnresolvedValue::from_variant_unambiguous(variant.clone()),
                ] {
                    let written = serde_json::to_string(&unresolved).unwrap();
                    assert_eq!(written, expected);
                    let read: UnresolvedValue = json::from_str(&written).unwrap();
                    assert!(
                        same(&read.clone().resolve(class, prop).unwrap(), &variant),
                        "{written}"
                    );
                    assert!(
                        same(&read.resolve_unambiguous().unwrap(), &variant),
                        "{written}"
                    );
                }
            }
        }
    }

    /// Builds one variant per float slot of `base`, with `value` in that slot.
    fn per_slot<const N: usize>(
        value: f32,
        base: [f32; N],
        build: impl Fn([f32; N]) -> Variant,
    ) -> Vec<Variant> {
        (0..N)
            .map(|slot| {
                let mut components = base;
                components[slot] = value;
                build(components)
            })
            .collect()
    }

    /// Every composite type with a tagged form, once per float slot.
    fn composites(value: f32) -> Vec<Variant> {
        let v3 = |c: &[f32]| Vector3::new(c[0], c[1], c[2]);
        let cframe = |c: [f32; 12]| {
            CFrame::new(
                v3(&c[0..3]),
                Matrix3::new(v3(&c[3..6]), v3(&c[6..9]), v3(&c[9..12])),
            )
        };
        let cframe_base = [1.0, 2.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        [
            per_slot(value, [1.0, 2.0], |c| Vector2::new(c[0], c[1]).into()),
            per_slot(value, [1.0, 2.0, 3.0], |c| v3(&c).into()),
            per_slot(value, [0.1, 0.2, 0.3], |c| {
                Color3::new(c[0], c[1], c[2]).into()
            }),
            per_slot(value, cframe_base, |c| cframe(c).into()),
            per_slot(value, cframe_base, |c| {
                Variant::OptionalCFrame(Some(cframe(c)))
            }),
            per_slot(value, [0.0, 1.0], |c| NumberRange::new(c[0], c[1]).into()),
            per_slot(value, [0.0, 0.5, 0.1, 1.0, 0.7, 0.2], |c| {
                NumberSequence {
                    keypoints: vec![
                        NumberSequenceKeypoint::new(c[0], c[1], c[2]),
                        NumberSequenceKeypoint::new(c[3], c[4], c[5]),
                    ],
                }
                .into()
            }),
            per_slot(value, [0.0, 0.1, 0.2, 0.3, 1.0, 0.4, 0.5, 0.6], |c| {
                ColorSequence {
                    keypoints: vec![
                        ColorSequenceKeypoint::new(c[0], Color3::new(c[1], c[2], c[3])),
                        ColorSequenceKeypoint::new(c[4], Color3::new(c[5], c[6], c[7])),
                    ],
                }
                .into()
            }),
            per_slot(value, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0], |c| {
                Ray::new(v3(&c[0..3]), v3(&c[3..6])).into()
            }),
            per_slot(value, [1.0, 2.0, 3.0, 4.0], |c| {
                Rect::new(Vector2::new(c[0], c[1]), Vector2::new(c[2], c[3])).into()
            }),
            per_slot(value, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0], |c| {
                Region3::new(v3(&c[0..3]), v3(&c[3..6])).into()
            }),
            per_slot(value, [0.5], |c| UDim::new(c[0], 7).into()),
            per_slot(value, [0.5, 0.25], |c| {
                UDim2::new(UDim::new(c[0], 7), UDim::new(c[1], -3)).into()
            }),
        ]
        .concat()
    }

    /// Every non-finite value in every float slot of every composite type is
    /// detected, written as a tagged string, and read back into the same slot,
    /// through both the property and attribute writers.
    #[test]
    fn non_finite_composites_round_trip_through_tagged_text() {
        for value in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            for variant in composites(value) {
                for unresolved in [
                    UnresolvedValue::from_variant(variant.clone(), "Folder", "NotAProperty"),
                    UnresolvedValue::from_variant_unambiguous(variant.clone()),
                ] {
                    assert!(
                        matches!(unresolved, UnresolvedValue::NonFinite(_)),
                        "{variant:?} was not detected as non-finite"
                    );
                    let written = serde_json::to_string(&unresolved).unwrap();
                    assert!(!written.contains("null"), "{written}");
                    let read: UnresolvedValue = json::from_str(&written).unwrap();
                    let resolved = read.resolve_unambiguous().unwrap();
                    assert!(
                        variant_eq(&resolved, &variant),
                        "{written} read back as {resolved:?}"
                    );
                }
            }
        }
    }

    /// The tagged form must keep the exact shape of the fully qualified form,
    /// so finite values encoded both ways must write the same JSON.
    #[test]
    fn tagged_composites_match_the_fully_qualified_shape() {
        for variant in composites(0.75) {
            let tagged = Encoder::default().encode(&variant).unwrap();
            assert_eq!(
                serde_json::to_value(&tagged).unwrap(),
                serde_json::to_value(&variant).unwrap(),
                "{variant:?}"
            );
            assert!(NonFiniteValue::from_variant(&variant).is_none());
        }
    }

    #[test]
    fn non_finite_composites_are_written_readably() {
        let range =
            UnresolvedValue::from_variant_unambiguous(NumberRange::new(0.0, f32::INFINITY).into());
        assert_eq!(
            serde_json::to_string(&range).unwrap(),
            r#"{"NumberRange":[0.0,"inf"]}"#
        );

        // Properties known to the reflection database are normally written in
        // an ambiguous form, which can't hold infinity.
        let max_size = UnresolvedValue::from_variant(
            Vector2::new(300.0, f32::INFINITY).into(),
            "UISizeConstraint",
            "MaxSize",
        );
        let written = serde_json::to_string(&max_size).unwrap();
        assert_eq!(written, r#"{"Vector2":[300.0,"inf"]}"#);
        assert_eq!(
            resolve("UISizeConstraint", "MaxSize", &written),
            Variant::Vector2(Vector2::new(300.0, f32::INFINITY))
        );
    }

    /// A plain string must never be mistaken for a tagged non-finite number.
    #[test]
    fn non_finite_spellings_stay_strings_when_untagged() {
        for text in ["inf", "-inf", "nan"] {
            assert_eq!(
                resolve_unambiguous(&format!("\"{text}\"")),
                Variant::String(text.into())
            );
        }
        assert_eq!(resolve_unambiguous("1e308"), Variant::Float64(1e308));
    }

    #[test]
    fn bools() {
        assert_eq!(resolve("BoolValue", "Value", "false"), Variant::Bool(false));

        // Script.Disabled is inherited from BaseScript
        assert_eq!(resolve("Script", "Disabled", "true"), Variant::Bool(true));

        assert_eq!(resolve_unambiguous("false"), Variant::Bool(false));
        assert_eq!(resolve_unambiguous("true"), Variant::Bool(true));
    }

    #[test]
    fn strings() {
        // String literals can stay as strings
        assert_eq!(
            resolve("StringValue", "Value", "\"Hello!\""),
            Variant::String("Hello!".into()),
        );

        // String literals can also turn into ContentId
        assert_eq!(
            resolve("Sky", "MoonTextureId", "\"rbxassetid://12345\""),
            Variant::ContentId("rbxassetid://12345".into()),
        );

        // String literals can turn into Content!
        assert_eq!(
            resolve(
                "MeshPart",
                "MeshContent",
                "\"rbxasset://totally-a-real-uri.tiff\""
            ),
            Variant::Content("rbxasset://totally-a-real-uri.tiff".into())
        );

        // What about BinaryString values? For forward-compatibility reasons, we
        // don't support any shorthands for BinaryString.
        //
        // assert_eq!(
        //     resolve("Folder", "Tags", "\"a\\u0000b\\u0000c\""),
        //     Variant::BinaryString(b"a\0b\0c".to_vec().into()),
        // );

        assert_eq!(
            resolve_unambiguous("\"Hello world!\""),
            Variant::String("Hello world!".into()),
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            resolve("Part", "CollisionGroupId", "123"),
            Variant::Int32(123),
        );

        assert_eq!(
            resolve("IntValue", "Value", "532413"),
            Variant::Int64(532413),
        );

        assert_eq!(resolve("Part", "Transparency", "1"), Variant::Float32(1.0));
        assert_eq!(resolve("NumberValue", "Value", "1"), Variant::Float64(1.0));

        assert_eq!(resolve_unambiguous("12.5"), Variant::Float64(12.5));
    }

    #[test]
    fn vectors() {
        assert_eq!(
            resolve("ParticleEmitter", "SpreadAngle", "[1, 2]"),
            Variant::Vector2(Vector2::new(1.0, 2.0)),
        );

        assert_eq!(
            resolve("Part", "Position", "[4, 5, 6]"),
            Variant::Vector3(Vector3::new(4.0, 5.0, 6.0)),
        );
    }

    #[test]
    fn colors() {
        assert_eq!(
            resolve("Part", "Color", "[1, 1, 1]"),
            Variant::Color3(Color3::new(1.0, 1.0, 1.0)),
        );

        // There aren't any user-facing Color3uint8 properties. If there are
        // some, we should treat them the same in the future.
    }

    #[test]
    fn enums() {
        assert_eq!(
            resolve("Lighting", "Technology", "\"Voxel\""),
            Variant::Enum(Enum::from_u32(1)),
        );
    }

    #[test]
    fn font() {
        use rbx_dom_weak::types::{FontStyle, FontWeight};

        assert_eq!(
            resolve(
                "TextLabel",
                "FontFace",
                r#"{"family": "rbxasset://fonts/families/RobotoMono.json", "weight": "Thin", "style": "Normal"}"#
            ),
            Variant::Font(Font {
                family: "rbxasset://fonts/families/RobotoMono.json".into(),
                weight: FontWeight::Thin,
                style: FontStyle::Normal,
                cached_face_id: None,
            })
        )
    }

    #[test]
    fn material_colors() {
        use rbx_dom_weak::types::{Color3uint8, TerrainMaterials};

        let mut material_colors = MaterialColors::new();
        material_colors.set_color(TerrainMaterials::Grass, Color3uint8::new(10, 20, 30));
        material_colors.set_color(TerrainMaterials::Asphalt, Color3uint8::new(40, 50, 60));
        material_colors.set_color(TerrainMaterials::LeafyGrass, Color3uint8::new(255, 155, 55));

        assert_eq!(
            resolve(
                "Terrain",
                "MaterialColors",
                r#"{
                    "Grass": [10, 20, 30],
                    "Asphalt": [40, 50, 60],
                    "LeafyGrass": [255, 155, 55]
                }"#
            ),
            Variant::MaterialColors(material_colors)
        )
    }
}
