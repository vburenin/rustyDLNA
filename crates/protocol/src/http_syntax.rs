//! Shared HTTP/1.x line syntax used by HTTP and SSDP.
//!
//! These helpers validate line safety and token grammar. They deliberately do
//! not assign semantics to fields such as `Date`, `Server`, or SSDP UUIDs.

const fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Return whether `value` is a non-empty HTTP field-name or method token.
pub fn is_http_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_token_byte)
}

/// Return whether `value` can be emitted as one HTTP field value or reason.
///
/// Empty values, visible bytes, spaces, horizontal tabs, and UTF-8/HTTP
/// `obs-text` bytes are accepted. CR, LF, other C0 controls, and DEL are not.
pub fn is_http_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || byte >= b' ' && byte != 0x7f)
}

/// Trim HTTP optional whitespace (SP and HTAB), but no other whitespace.
pub fn trim_http_ows(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_accepts_every_tchar_and_rejects_separators() {
        assert!(is_http_token("azAZ09!#$%&'*+-.^_`|~"));
        for rejected in ["", "has space", "name:value", "name/value", "café"] {
            assert!(!is_http_token(rejected), "{rejected:?}");
        }
    }

    #[test]
    fn field_values_allow_http_text_but_never_line_controls() {
        for accepted in ["", "value", " value\t", "café"] {
            assert!(is_http_field_value(accepted), "{accepted:?}");
        }
        for rejected in ["a\0b", "a\nb", "a\rb", "a\u{b}b", "a\u{7f}b"] {
            assert!(!is_http_field_value(rejected), "{rejected:?}");
        }
    }

    #[test]
    fn ows_is_only_space_and_horizontal_tab() {
        assert_eq!(trim_http_ows(" \t value\t "), "value");
        assert_eq!(trim_http_ows("\u{a0}value\u{a0}"), "\u{a0}value\u{a0}");
        assert_eq!(trim_http_ows("\nvalue\r"), "\nvalue\r");
    }
}
