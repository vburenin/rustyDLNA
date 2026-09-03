#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_soap::{
    parse_filter, try_parse_search_criteria, try_parse_soap_call, xml_escape, xml_tag_text,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = try_parse_soap_call(text, text);
    let _ = try_parse_search_criteria(Some(text));
    let _ = parse_filter(Some(text), data.first().is_some_and(|byte| byte & 1 == 1));
    let _ = xml_tag_text(text, "ObjectID");
    let escaped = xml_escape(text);
    assert!(escaped.len() >= text.len());
});
