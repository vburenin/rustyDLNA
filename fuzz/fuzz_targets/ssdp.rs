#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_ssdp::{parse_inbound_notify, parse_msearch};

fuzz_target!(|data: &[u8]| {
    let Ok(packet) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(search) = parse_msearch(packet) {
        assert!(!search.st.is_empty());
        assert!(search.mx >= 0);
    }
    if let Some(notify) = parse_inbound_notify(packet) {
        assert!(!notify.nt.is_empty());
    }
});
