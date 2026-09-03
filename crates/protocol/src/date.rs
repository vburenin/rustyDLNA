//! `dc:date` helpers from `src/utils.c`.
//!
//! Kodi Platinum `FORMAT_W3C` rejects a 19-character `YYYY-MM-DDTHH:MM:SS`
//! (needs a timezone, length >= 20, or a 10-character day). A failed parse
//! clears the date and Kodi shows year 1905.

/// UTC calendar fields derived from a Unix timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UtcDateTime {
    /// Proleptic Gregorian year.
    pub year: i64,
    /// Month in `1..=12`.
    pub month: u8,
    /// Day of month in `1..=31`.
    pub day: u8,
    /// Hour in `0..=23`.
    pub hour: u8,
    /// Minute in `0..=59`.
    pub minute: u8,
    /// Second in `0..=59`.
    pub second: u8,
    /// Weekday index where Sunday is zero.
    pub weekday_from_sunday: u8,
}

/// Convert a Unix timestamp to UTC without accepting or parsing untrusted text.
///
/// The civil-date transform is Howard Hinnant's era-based Gregorian algorithm.
/// Euclidean division keeps timestamps before 1970 correct as well.
pub fn utc_date_time(unix: i64) -> UtcDateTime {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    UtcDateTime {
        year,
        month: month as u8,
        day: day as u8,
        hour: (seconds / 3_600) as u8,
        minute: ((seconds % 3_600) / 60) as u8,
        second: (seconds % 60) as u8,
        weekday_from_sunday: (days + 4).rem_euclid(7) as u8,
    }
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ` from a Unix timestamp (`w3c_date_from_time`).
pub fn w3c_date_from_unix(unix: i64) -> Option<String> {
    let t = utc_date_time(unix);
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second
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
    fn unix_conversion_handles_boundaries_and_negative_values() {
        assert_eq!(
            utc_date_time(-1),
            UtcDateTime {
                year: 1969,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 59,
                weekday_from_sunday: 3,
            }
        );
        assert_eq!(
            w3c_date_from_unix(951_782_400).unwrap(),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            w3c_date_from_unix(1_704_067_199).unwrap(),
            "2023-12-31T23:59:59Z"
        );
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
