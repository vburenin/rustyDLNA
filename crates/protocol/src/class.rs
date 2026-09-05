//! Catalog classes retain their stored short form. Search compares ASCII case
//! insensitively using the full DIDL value; substring needles remain literal.

use std::borrow::Cow;

pub const OBJECT_CLASS_PREFIX: &str = "object.";

pub fn full_object_class(value: &str) -> Cow<'_, str> {
    if value.is_empty()
        || value
            .get(..OBJECT_CLASS_PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(OBJECT_CLASS_PREFIX))
    {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(format!("{OBJECT_CLASS_PREFIX}{value}"))
    }
}

pub fn object_class_derived_from(value: &str, ancestor: &str) -> bool {
    let value = full_object_class(value).to_ascii_lowercase();
    let ancestor = full_object_class(ancestor).to_ascii_lowercase();
    value == ancestor
        || value
            .strip_prefix(&ancestor)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_classes_preserve_empty_and_normalize_short_case_variants() {
        assert_eq!(full_object_class(""), "");
        assert_eq!(full_object_class("item.videoItem"), "object.item.videoItem");
        assert_eq!(full_object_class("OBJECT.ITEM"), "OBJECT.ITEM");
        assert!(object_class_derived_from("item.videoItem", "OBJECT.ITEM"));
        assert!(object_class_derived_from(
            "OBJECT.ITEM.VIDEOITEM",
            "item.videoItem"
        ));
        assert!(!object_class_derived_from(
            "item.videoItemExtra",
            "item.videoItem"
        ));
        assert!(!object_class_derived_from("container", "object.item"));
        assert!(!object_class_derived_from("item.videoItem", ""));
        assert_eq!(full_object_class("éééé"), "object.éééé");
    }
}
