//! XML 1.0 scalar sanitization and escaping.
//!
//! All rustyDLNA XML emitters use these helpers so untrusted catalog and
//! configuration text cannot introduce characters forbidden by XML 1.0.

const REPLACEMENT_CHARACTER: char = '\u{fffd}';

/// Escape XML markup and both quote characters, replacing XML-1.0-invalid
/// Unicode scalars with U+FFFD.
///
/// This form preserves rustyDLNA's existing attribute-safe wire encoding and
/// is also used for element text where quotes have historically been escaped.
pub fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    escape_xml_into(value, &mut escaped);
    escaped
}

/// Append [`escape_xml`] output to an existing buffer.
pub fn escape_xml_into(value: &str, escaped: &mut String) {
    escape_into(value, escaped, true);
}

/// Escape XML text markup while leaving quote characters raw, replacing
/// XML-1.0-invalid Unicode scalars with U+FFFD.
///
/// This is the compatibility form for DIDL serialized inside a SOAP
/// `<Result>` value. Some renderer XML stacks require the quotes in the
/// escaped DIDL attributes to remain literal.
pub fn escape_xml_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    escape_xml_text_into(value, &mut escaped);
    escaped
}

/// Append [`escape_xml_text`] output to an existing buffer.
pub fn escape_xml_text_into(value: &str, escaped: &mut String) {
    escape_into(value, escaped, false);
}

fn escape_into(value: &str, escaped: &mut String, escape_quotes: bool) {
    for scalar in value.chars() {
        match scalar {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' if escape_quotes => escaped.push_str("&quot;"),
            '\'' if escape_quotes => escaped.push_str("&apos;"),
            scalar if is_xml_1_0_scalar(scalar) => escaped.push(scalar),
            _ => escaped.push(REPLACEMENT_CHARACTER),
        }
    }
}

const fn is_xml_1_0_scalar(scalar: char) -> bool {
    matches!(
        scalar,
        '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_safe_form_escapes_markup_and_quotes() {
        assert_eq!(
            escape_xml("<&>\"' Björk 🦀"),
            "&lt;&amp;&gt;&quot;&apos; Björk 🦀"
        );
    }

    #[test]
    fn text_form_preserves_didl_attribute_quotes() {
        assert_eq!(
            escape_xml_text("<container id=\"2$8\" owner='A&B'/>>"),
            "&lt;container id=\"2$8\" owner='A&amp;B'/&gt;&gt;"
        );
    }

    #[test]
    fn every_invalid_c0_control_is_replaced_in_both_forms() {
        for value in 0_u32..=0x1f {
            let control = char::from_u32(value).expect("C0 scalar");
            let input = format!("before{control}after");
            let expected = if matches!(control, '\t' | '\n' | '\r') {
                input.as_str()
            } else {
                "before\u{fffd}after"
            };
            assert_eq!(escape_xml(&input), expected, "U+{value:04X}");
            assert_eq!(escape_xml_text(&input), expected, "U+{value:04X}");
        }
    }

    #[test]
    fn invalid_noncharacters_are_replaced_and_xml_boundaries_are_preserved() {
        assert_eq!(escape_xml("\u{fffe}\u{ffff}"), "\u{fffd}\u{fffd}");
        assert_eq!(
            escape_xml("\u{7f}\u{85}\u{d7ff}\u{e000}\u{fffd}\u{10000}\u{10ffff}"),
            "\u{7f}\u{85}\u{d7ff}\u{e000}\u{fffd}\u{10000}\u{10ffff}"
        );
    }

    #[test]
    fn into_forms_append_without_changing_the_prefix() {
        let mut attribute = "prefix:".to_owned();
        escape_xml_into("<&\u{1}", &mut attribute);
        assert_eq!(attribute, "prefix:&lt;&amp;\u{fffd}");

        let mut text = "prefix:".to_owned();
        escape_xml_text_into("<&\"\u{1}", &mut text);
        assert_eq!(text, "prefix:&lt;&amp;\"\u{fffd}");
    }
}
