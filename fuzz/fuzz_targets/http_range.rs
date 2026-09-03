#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_http::{parse_byte_range, parse_open_range, range_len};

fuzz_target!(|data: &[u8]| {
    let (prefix, raw) = data.split_at(data.len().min(8));
    let mut size_bytes = [0_u8; 8];
    size_bytes[..prefix.len()].copy_from_slice(prefix);
    let size = u64::from_le_bytes(size_bytes);
    let Ok(value) = std::str::from_utf8(raw) else {
        return;
    };

    if let Ok(Some(range)) = parse_byte_range(value, size) {
        assert!(size > 0);
        assert!(range.start <= range.end);
        assert!(range.end < size);
        assert_eq!(range_len(range), range.end - range.start + 1);
    }
    if let Ok((start, end)) = parse_open_range(value) {
        assert!(end.map(|end| start <= end).unwrap_or(true));
    }
});
