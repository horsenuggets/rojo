use rbx_dom_weak::{
    types::{ContentType, Variant},
    Instance, Ustr, UstrMap,
};
use rbx_reflection::{PropertyKind, PropertySerialization, Scriptability};

use crate::{resolution::normalize_json_variant, variant_eq::variant_eq, Project};

/// Returns a map of properties from `inst` that are both allowed under the
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties<'inst>(
    project: &Project,
    inst: &'inst Instance,
) -> UstrMap<&'inst Variant> {
    let mut map: Vec<(Ustr, &Variant)> = Vec::with_capacity(inst.properties.len());
    filter_json_properties_preallocated(project, inst, &mut map);

    map.into_iter().collect()
}

/// Fills `allocation` with a list of properties from `inst` that are
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties_preallocated<'inst>(
    project: &Project,
    inst: &'inst Instance,
    allocation: &mut Vec<(Ustr, &'inst Variant)>,
) {
    filter_properties_impl(project, inst, allocation, false);
}

/// Filters JSON properties after numeric normalization, so rounded defaults are
/// omitted on the first export rather than disappearing on the next export.
pub fn filter_json_properties_preallocated<'inst>(
    project: &Project,
    inst: &'inst Instance,
    allocation: &mut Vec<(Ustr, &'inst Variant)>,
) {
    filter_properties_impl(project, inst, allocation, true);
}

fn filter_properties_impl<'inst>(
    project: &Project,
    inst: &'inst Instance,
    allocation: &mut Vec<(Ustr, &'inst Variant)>,
    normalize_defaults: bool,
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
                let normalized;
                let comparable = if normalize_defaults {
                    normalized = normalize_json_variant(value.clone());
                    &normalized
                } else {
                    value
                };
                if !variant_eq(comparable, default) {
                    allocation.push((*name, value));
                }
            } else if !is_unset_value(value) {
                allocation.push((*name, value));
            }
        }
    } else {
        for (name, value) in &inst.properties {
            if predicate(name, value) || is_unset_value(value) {
                continue;
            }
            allocation.push((*name, value));
        }
    }
}

/// Returns whether `value` holds nothing: an empty `BinaryString` or an unset
/// `Content`. Newer Studio versions serialize such properties (like
/// `Instance.SerializedOverrides` and `Folder.ExplorerIconContent`) on every
/// instance before the reflection database records a default for them, so
/// without this check syncback would write an empty value into the metadata
/// of every instance.
pub(crate) fn is_unset_value(value: &Variant) -> bool {
    match value {
        Variant::BinaryString(bytes) => AsRef::<[u8]>::as_ref(bytes).is_empty(),
        Variant::Content(content) => matches!(content.value(), ContentType::None),
        _ => false,
    }
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
    use rbx_dom_weak::{
        types::{BinaryString, CFrame, Content, Matrix3, Vector2, Vector3},
        ustr, InstanceBuilder, WeakDom,
    };

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

    #[test]
    fn json_filter_omits_rounded_defaults_without_changing_binary_hash_filtering() {
        let (_temp, project) = load_project();
        let name = ustr("Transparency");
        for (value, kept) in [(0.0000005, false), (0.000002, true)] {
            let dom = WeakDom::new(
                InstanceBuilder::new("Part").with_property(name, Variant::Float32(value)),
            );
            assert_eq!(
                filter_properties(&project, dom.root()).contains_key(&name),
                kept
            );
            let mut raw = Vec::new();
            filter_properties_preallocated(&project, dom.root(), &mut raw);
            assert_eq!(raw, vec![(name, &Variant::Float32(value))]);
        }
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

    /// Walks every combination of a class with and without reflection data and
    /// a content property with and without a value.
    #[test]
    fn content_without_defaults_is_kept_only_when_set() {
        let (_temp, project) = load_project();
        let name = ustr("ExplorerIconContent");
        for class in ["Folder", "NotARealClass"] {
            for (content, kept) in [
                (Content::none(), false),
                (Content::from_uri("rbxassetid://1"), true),
            ] {
                let dom = WeakDom::new(InstanceBuilder::new(class).with_property(name, content));
                assert_eq!(
                    filter_properties(&project, dom.root()).contains_key(&name),
                    kept,
                    "{class} keeping {kept}"
                );
            }
        }
    }

    /// Non-finite composite values are kept like any other value, so they're
    /// only dropped when they match a default. `UISizeConstraint.MaxSize`
    /// defaults to `(inf, inf)`, and Roblox leaves `Model.ModelMeshCFrame` NaN.
    #[test]
    fn non_finite_composites_are_kept_unless_they_are_the_default() {
        let (_temp, project) = load_project();
        let cases = [
            (
                "UISizeConstraint",
                "MaxSize",
                Vector2::new(f32::INFINITY, f32::INFINITY).into(),
                false,
            ),
            (
                "UISizeConstraint",
                "MaxSize",
                Vector2::new(300.0, f32::INFINITY).into(),
                true,
            ),
            (
                "UISizeConstraint",
                "MaxSize",
                Vector2::new(f32::MAX, f32::MAX).into(),
                true,
            ),
            (
                "Model",
                "ModelMeshCFrame",
                Variant::CFrame(CFrame::new(
                    Vector3::new(f32::NAN, f32::NAN, f32::NAN),
                    Matrix3::identity(),
                )),
                true,
            ),
        ];
        for (class, prop, value, kept) in cases {
            let name = ustr(prop);
            let dom = WeakDom::new(InstanceBuilder::new(class).with_property(name, value.clone()));
            assert_eq!(
                filter_properties(&project, dom.root()).contains_key(&name),
                kept,
                "{class}.{prop} = {value:?}"
            );
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
