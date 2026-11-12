use rbx_dom_weak::{
    types::{CFrame, Variant, Vector2, Vector3},
    Instance, Ustr, UstrMap,
};
use rbx_reflection::{PropertyKind, PropertySerialization, Scriptability};

use crate::{variant_eq::variant_eq, Project};

/// Returns a map of properties from `inst` that are both allowed under the
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties<'inst>(
    project: &Project,
    inst: &'inst Instance,
) -> UstrMap<&'inst Variant> {
    let mut map: Vec<(Ustr, &Variant)> = Vec::with_capacity(inst.properties.len());
    filter_properties_preallocated(project, inst, &mut map);

    map.into_iter().collect()
}

/// Fills `allocation` with a list of properties from `inst` that are
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties_preallocated<'inst>(
    project: &Project,
    inst: &'inst Instance,
    allocation: &mut Vec<(Ustr, &'inst Variant)>,
) {
    let sync_unscriptable = project
        .syncback_rules
        .as_ref()
        .and_then(|s| s.sync_unscriptable)
        .unwrap_or(true);

    let class_data = rbx_reflection_database::get()
        .unwrap()
        .classes
        .get(inst.class.as_str());

    let predicate = |prop_name: &Ustr, prop_value: &Variant| {
        // JSON has no representation for NaN or infinity. Roblox uses NaN
        // CFrames for unset values such as Model.ModelMeshCFrame, so leaving
        // them in would make the metadata unreadable.
        if !is_json_compatible(prop_value) {
            return true;
        }
        // We don't want to serialize Ref or UniqueId properties in JSON files
        if matches!(prop_value, Variant::Ref(_) | Variant::UniqueId(_)) {
            return true;
        }
        if !should_property_serialize(&inst.class, prop_name) {
            return true;
        }
        if !sync_unscriptable {
            if let Some(data) = class_data {
                if let Some(prop_data) = data.properties.get(prop_name.as_str()) {
                    if matches!(prop_data.scriptability, Scriptability::None) {
                        return true;
                    }
                }
            }
        }
        false
    };

    if let Some(class_data) = class_data {
        let defaults = &class_data.default_properties;
        for (name, value) in &inst.properties {
            if predicate(name, value) {
                continue;
            }
            if let Some(default) = defaults.get(name.as_str()) {
                if !variant_eq(value, default) {
                    allocation.push((*name, value));
                }
            } else {
                allocation.push((*name, value));
            }
        }
    } else {
        for (name, value) in &inst.properties {
            if predicate(name, value) {
                continue;
            }
            allocation.push((*name, value));
        }
    }
}

/// Returns whether a value can be represented in JSON without `null` from
/// non-finite numbers. Roblox uses NaN and infinity for some unset values.
pub fn is_json_compatible(value: &Variant) -> bool {
    match value {
        Variant::Float32(value) => value.is_finite(),
        Variant::Float64(value) => value.is_finite(),
        Variant::Vector2(value) => vector2_is_finite(value),
        Variant::Vector3(value) => vector3_is_finite(value),
        Variant::Color3(value) => [value.r, value.g, value.b].into_iter().all(f32::is_finite),
        Variant::CFrame(value) => cframe_is_finite(value),
        Variant::OptionalCFrame(value) => value.as_ref().is_some_and(cframe_is_finite),
        Variant::NumberRange(value) => [value.min, value.max].into_iter().all(f32::is_finite),
        Variant::NumberSequence(value) => value.keypoints.iter().all(|point| {
            [point.time, point.value, point.envelope]
                .into_iter()
                .all(f32::is_finite)
        }),
        Variant::ColorSequence(value) => value.keypoints.iter().all(|point| {
            point.time.is_finite()
                && [point.color.r, point.color.g, point.color.b]
                    .into_iter()
                    .all(f32::is_finite)
        }),
        Variant::Ray(value) => {
            vector3_is_finite(&value.origin) && vector3_is_finite(&value.direction)
        }
        Variant::Rect(value) => vector2_is_finite(&value.min) && vector2_is_finite(&value.max),
        Variant::Region3(value) => vector3_is_finite(&value.min) && vector3_is_finite(&value.max),
        Variant::UDim(value) => value.scale.is_finite(),
        Variant::UDim2(value) => value.x.scale.is_finite() && value.y.scale.is_finite(),
        // Attribute bags are filtered per entry when serialized and hashed so
        // one non-finite attribute does not discard all the valid ones.
        Variant::Attributes(_) => true,
        _ => true,
    }
}

fn vector2_is_finite(vector: &Vector2) -> bool {
    vector.x.is_finite() && vector.y.is_finite()
}

fn vector3_is_finite(vector: &Vector3) -> bool {
    vector.x.is_finite() && vector.y.is_finite() && vector.z.is_finite()
}

fn cframe_is_finite(cframe: &CFrame) -> bool {
    let position = cframe.position;
    let orientation = cframe.orientation;
    [
        position.x,
        position.y,
        position.z,
        orientation.x.x,
        orientation.x.y,
        orientation.x.z,
        orientation.y.x,
        orientation.y.y,
        orientation.y.z,
        orientation.z.x,
        orientation.z.y,
        orientation.z.z,
    ]
    .into_iter()
    .all(f32::is_finite)
}

fn should_property_serialize(class_name: &str, prop_name: &str) -> bool {
    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = class_name;

    loop {
        let class_data = match database.classes.get(current_class_name) {
            Some(data) => data,
            None => return true,
        };
        if let Some(data) = class_data.properties.get(prop_name) {
            log::trace!("found {class_name}.{prop_name} on {current_class_name}");
            return match &data.kind {
                // It's not really clear if this can ever happen but I want to
                // support it just in case!
                PropertyKind::Alias { alias_for } => {
                    should_property_serialize(current_class_name, alias_for)
                }
                // Migrations and aliases are happily handled for us by parsers
                // so we don't really need to handle them.
                PropertyKind::Canonical { serialization } => {
                    !matches!(serialization, PropertySerialization::DoesNotSerialize)
                }
                kind => unimplemented!("unknown property kind {kind:?}"),
            };
        } else if let Some(super_class) = class_data.superclass {
            current_class_name = super_class;
        } else {
            break;
        }
    }
    true
}
