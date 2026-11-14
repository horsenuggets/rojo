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
        // JSON has no representation for NaN or infinity inside composite
        // values. Roblox uses NaN CFrames for unset values such as
        // Model.ModelMeshCFrame, so leaving them in would make the metadata
        // unreadable.
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
            } else if !is_unset_binary_string(value) {
                allocation.push((*name, value));
            }
        }
    } else {
        for (name, value) in &inst.properties {
            if predicate(name, value) || is_unset_binary_string(value) {
                continue;
            }
            allocation.push((*name, value));
        }
    }
}

/// Returns whether `value` is an empty `BinaryString`, which Roblox writes for
/// binary properties that hold nothing. Newer Studio versions serialize such
/// properties (like `Instance.SerializedOverrides`) on every instance before
/// the reflection database records a default for them, so without this check
/// syncback would write an empty value into the metadata of every instance.
fn is_unset_binary_string(value: &Variant) -> bool {
    matches!(value, Variant::BinaryString(bytes) if AsRef::<[u8]>::as_ref(bytes).is_empty())
}

/// Returns whether a value can be represented in JSON without `null` from
/// non-finite numbers. Roblox uses NaN and infinity for some unset values.
/// Plain numbers always can, since non-finite ones are written in a tagged
/// form like `{"Float64": "inf"}`.
pub fn is_json_compatible(value: &Variant) -> bool {
    match value {
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

pub(crate) fn can_write_property(project: &Project, class_name: &str, prop_name: &str) -> bool {
    if !should_property_serialize(class_name, prop_name) {
        return false;
    }
    if project
        .syncback_rules
        .as_ref()
        .and_then(|rules| rules.sync_unscriptable)
        != Some(false)
    {
        return true;
    }
    let database = rbx_reflection_database::get().unwrap();
    let mut current_class = class_name;
    while let Some(class) = database.classes.get(current_class) {
        if let Some(property) = class.properties.get(prop_name) {
            return !matches!(property.scriptability, Scriptability::None);
        }
        match class.superclass {
            Some(parent) => current_class = parent,
            None => break,
        }
    }
    true
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

#[cfg(test)]
mod test {
    use super::*;
    use memofs::Vfs;
    use rbx_dom_weak::{types::BinaryString, ustr, InstanceBuilder, WeakDom};

    fn load_project() -> (tempfile::TempDir, Project) {
        let temp = tempfile::tempdir().unwrap();
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let path = temp.path().join("default.project.json");
        vfs.write(&path, r#"{"name":"Test","tree":{"$className":"Folder"}}"#)
            .unwrap();
        let project = Project::load_exact(&vfs, &path, Some("Test")).unwrap();
        (temp, project)
    }

    /// Walks every combination of a class with and without reflection data and
    /// a binary property with and without content.
    #[test]
    fn binary_strings_without_defaults_are_kept_only_when_non_empty() {
        let (_temp, project) = load_project();
        let name = ustr("SerializedOverrides");
        for class in ["Folder", "NotARealClass"] {
            for bytes in [Vec::new(), vec![1, 2, 3]] {
                let dom = WeakDom::new(
                    InstanceBuilder::new(class)
                        .with_property(name, BinaryString::from(bytes.clone())),
                );
                let filtered = filter_properties(&project, dom.root());
                assert_eq!(
                    filtered.contains_key(&name),
                    !bytes.is_empty(),
                    "{class} with {} bytes",
                    bytes.len()
                );
            }
        }
    }

    #[test]
    fn properties_with_defaults_still_compare_against_them() {
        let (_temp, project) = load_project();
        let name = ustr("Transparency");
        for (value, kept) in [(0.0, false), (0.5, true)] {
            let dom = WeakDom::new(
                InstanceBuilder::new("Part").with_property(name, Variant::Float32(value)),
            );
            assert_eq!(
                filter_properties(&project, dom.root()).contains_key(&name),
                kept,
                "Transparency {value}"
            );
        }
    }
}
