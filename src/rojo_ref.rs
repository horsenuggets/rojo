use std::{borrow::Cow, fmt, sync::Arc};

use rbx_dom_weak::types::{Attributes, Variant};
use serde::{Deserialize, Serialize};

pub const REF_ID_ATTRIBUTE_NAME: &str = "Rojo_Id";
pub const REF_POINTER_ATTRIBUTE_PREFIX: &str = "Rojo_Target_";

/// Whether an attribute only exists to link Ref properties on the filesystem.
/// Serve resolves these into real Ref properties, so Studio never needs them.
pub fn is_link_attribute(name: &str) -> bool {
    name == REF_ID_ATTRIBUTE_NAME || name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX)
}

/// Returns `value` without link attributes, borrowing it when there are none.
pub fn without_link_attributes(value: &Variant) -> Cow<'_, Variant> {
    match value {
        Variant::Attributes(attributes)
            if attributes.iter().any(|(name, _)| is_link_attribute(name)) =>
        {
            let kept: Attributes = attributes
                .iter()
                .filter(|(name, _)| !is_link_attribute(name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            Cow::Owned(Variant::Attributes(kept))
        }
        _ => Cow::Borrowed(value),
    }
}

/// Combines attributes edited in Studio with the link attributes the instance
/// already has. Studio never sees link attributes, so its values for them are
/// ignored and the existing ones are kept so references on disk stay intact.
pub fn merge_link_attributes(studio: &Variant, existing: Option<&Variant>) -> Variant {
    let Variant::Attributes(studio) = studio else {
        return studio.clone();
    };
    let mut merged: Attributes = studio
        .iter()
        .filter(|(name, _)| !is_link_attribute(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if let Some(Variant::Attributes(existing)) = existing {
        for (name, value) in existing.iter() {
            if is_link_attribute(name) {
                merged.insert(name.clone(), value.clone());
            }
        }
    }
    Variant::Attributes(merged)
}

// TODO add an internment strategy for RojoRefs
// Something like what rbx-dom does for SharedStrings probably works

#[derive(Debug, Default, PartialEq, Hash, Clone, Serialize, Deserialize, Eq)]
pub struct RojoRef(Arc<String>);

impl RojoRef {
    #[inline]
    pub fn new(id: String) -> Self {
        Self(Arc::from(id))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for RojoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn attributes(entries: &[(&str, &str)]) -> Variant {
        Variant::Attributes(
            entries
                .iter()
                .map(|(name, value)| (name.to_string(), Variant::String(value.to_string())))
                .collect(),
        )
    }

    fn names(value: &Variant) -> Vec<String> {
        let Variant::Attributes(attributes) = value else {
            panic!("expected attributes");
        };
        let mut names: Vec<_> = attributes.iter().map(|(name, _)| name.clone()).collect();
        names.sort();
        names
    }

    #[test]
    fn link_attributes_are_only_rojo_ids_and_targets() {
        assert!(is_link_attribute("Rojo_Id"));
        assert!(is_link_attribute("Rojo_Target_PrimaryPart"));
        assert!(!is_link_attribute("Rojo_Other"));
        assert!(!is_link_attribute("rojo_id"));
        assert!(!is_link_attribute("Health"));
    }

    #[test]
    fn stripping_borrows_unless_link_attributes_are_present() {
        let plain = attributes(&[("Health", "100")]);
        assert!(matches!(without_link_attributes(&plain), Cow::Borrowed(_)));

        let not_attributes = Variant::Float32(1.0);
        assert!(matches!(
            without_link_attributes(&not_attributes),
            Cow::Borrowed(_)
        ));

        let linked = attributes(&[
            ("Health", "100"),
            ("Rojo_Id", "a"),
            ("Rojo_Target_PrimaryPart", "b"),
        ]);
        let stripped = without_link_attributes(&linked);
        assert!(matches!(stripped, Cow::Owned(_)));
        assert_eq!(names(&stripped), ["Health"]);
    }

    /// Walks every combination of Studio and existing attributes having user
    /// and link entries, checking that user entries always come from Studio and
    /// link entries always come from the existing instance.
    #[test]
    fn merging_takes_user_attributes_from_studio_and_links_from_existing() {
        for bits in 0..16u8 {
            let studio_user = bits & 1 != 0;
            let studio_link = bits & 2 != 0;
            let existing_user = bits & 4 != 0;
            let existing_link = bits & 8 != 0;

            let mut studio = Vec::new();
            if studio_user {
                studio.push(("FromStudio", "studio"));
            }
            if studio_link {
                studio.push(("Rojo_Id", "studio-id"));
            }
            let mut existing = Vec::new();
            if existing_user {
                existing.push(("FromDisk", "disk"));
            }
            if existing_link {
                existing.push(("Rojo_Id", "disk-id"));
            }

            let merged = merge_link_attributes(&attributes(&studio), Some(&attributes(&existing)));
            let Variant::Attributes(merged) = merged else {
                panic!("expected attributes");
            };

            assert_eq!(
                merged.get("FromStudio").is_some(),
                studio_user,
                "{bits:04b}"
            );
            assert!(merged.get("FromDisk").is_none(), "{bits:04b}");
            match merged.get("Rojo_Id") {
                Some(Variant::String(id)) => {
                    assert!(existing_link, "{bits:04b}");
                    assert_eq!(id, "disk-id", "{bits:04b}");
                }
                Some(_) => panic!("unexpected Rojo_Id type"),
                None => assert!(!existing_link, "{bits:04b}"),
            }
        }
    }

    #[test]
    fn merging_without_existing_attributes_drops_studio_links() {
        let merged = merge_link_attributes(
            &attributes(&[("Health", "1"), ("Rojo_Target_Value", "x")]),
            None,
        );
        assert_eq!(names(&merged), ["Health"]);

        let not_attributes = Variant::Float32(2.0);
        assert_eq!(merge_link_attributes(&not_attributes, None), not_attributes);
    }
}
