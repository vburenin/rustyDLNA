//! `dc:date` helpers from `src/utils.c`.
//!
//! Kodi Platinum `FORMAT_W3C` rejects a 19-character `YYYY-MM-DDTHH:MM:SS`
//! (needs a timezone, length >= 20, or a 10-character day). A failed parse
//! clears the date and Kodi shows year 1905.

use time::OffsetDateTime;

/// UTC `YYYY-MM-DDTHH:MM:SSZ` from a Unix timestamp (`w3c_date_from_time`).
pub fn w3c_date_from_unix(unix: i64) -> Option<String> {
    let t = OffsetDateTime::from_unix_timestamp(unix).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    ))
}

/// Emit-time normalize (`w3c_normalize_date`). Empty / None → empty.
pub fn w3c_normalize_date(date: &str) -> String {
    let n = date.len();
    if n == 0 {
        return String::new();
    }

    // Bare year from Kodi <year>1999</year>
    if n == 4 {
        let b = date.as_bytes();
        if (b[0] == b'1' || b[0] == b'2')
            && b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[3].is_ascii_digit()
        {
            return format!("{date}-01-01");
        }
    }

    // YYYY-MM-DDTHH:MM:SS or space-T, no timezone
    if n == 19 {
        let b = date.as_bytes();
        if b[4] == b'-'
            && b[7] == b'-'
            && (b[10] == b'T' || b[10] == b' ')
            && b[13] == b':'
            && b[16] == b':'
        {
            let mut out = date.to_string();
            out.replace_range(10..11, "T");
            out.push('Z');
            return out;
        }
        // EXIF "YYYY:MM:DD HH:MM:SS"
        if b[4] == b':' && b[7] == b':' && b[10] == b' ' && b[13] == b':' && b[16] == b':' {
            return format!(
                "{}-{}-{}T{}Z",
                &date[0..4],
                &date[5..7],
                &date[8..10],
                &date[11..19]
            );
        }
    }

    date.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_twenty_chars_with_z() {
        let s = w3c_date_from_unix(0).unwrap();
        assert_eq!(s, "1970-01-01T00:00:00Z");
        assert_eq!(s.len(), 20);
    }

    #[test]
    fn nineteen_char_datetime_gets_z() {
        assert_eq!(
            w3c_normalize_date("2024-03-15T14:30:00"),
            "2024-03-15T14:30:00Z"
        );
        assert_eq!(
            w3c_normalize_date("2024-03-15 14:30:00"),
            "2024-03-15T14:30:00Z"
        );
    }

    #[test]
    fn exif_colon_date() {
        assert_eq!(
            w3c_normalize_date("2024:03:15 14:30:00"),
            "2024-03-15T14:30:00Z"
        );
    }

    #[test]
    fn bare_year() {
        assert_eq!(w3c_normalize_date("1999"), "1999-01-01");
        assert_eq!(w3c_normalize_date("1999").len(), 10);
    }

    #[test]
    fn already_ok_passed_through() {
        assert_eq!(
            w3c_normalize_date("2024-03-15T14:30:00Z"),
            "2024-03-15T14:30:00Z"
        );
        assert_eq!(w3c_normalize_date("2024-03-15"), "2024-03-15");
    }

    #[test]
    fn empty() {
        assert_eq!(w3c_normalize_date(""), "");
    }
}
