use rbx_dom_weak::types::Variant;

use crate::variant_eq::variant_eq;

fn same_value(a: Option<&Variant>, b: Option<&Variant>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => variant_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyWriteDecision {
    Persist,
    AlreadySynchronized,
    RestoreFilesystem,
}

pub fn decide_property_write(
    baseline: Option<&Variant>,
    filesystem: Option<&Variant>,
    studio: Option<&Variant>,
) -> PropertyWriteDecision {
    if same_value(filesystem, studio) {
        PropertyWriteDecision::AlreadySynchronized
    } else if same_value(baseline, filesystem) {
        PropertyWriteDecision::Persist
    } else {
        PropertyWriteDecision::RestoreFilesystem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_presence_and_change_combination_preserves_filesystem_authority() {
        let a = Variant::String("a".into());
        let b = Variant::String("b".into());
        let c = Variant::String("c".into());
        for baseline in [None, Some(&a)] {
            for filesystem in [None, Some(&a), Some(&b)] {
                for studio in [None, Some(&a), Some(&c)] {
                    let expected = if same_value(filesystem, studio) {
                        PropertyWriteDecision::AlreadySynchronized
                    } else if same_value(baseline, filesystem) {
                        PropertyWriteDecision::Persist
                    } else {
                        PropertyWriteDecision::RestoreFilesystem
                    };
                    assert_eq!(
                        decide_property_write(baseline, filesystem, studio),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn equal_values_are_pure_decisions() {
        let value = Variant::Bool(true);
        assert_eq!(
            decide_property_write(Some(&value), Some(&value), Some(&value)),
            PropertyWriteDecision::AlreadySynchronized
        );
    }
}
