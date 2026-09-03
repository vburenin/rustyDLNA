#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_http::{header_block_complete, HttpRequest};

fuzz_target!(|data: &[u8]| {
    let Some(end) = header_block_complete(data) else {
        return;
    };
    assert!(end <= data.len());
    assert_eq!(&data[end - 4..end], b"\r\n\r\n");
    let Ok(header) = std::str::from_utf8(&data[..end]) else {
        return;
    };
    if let Ok(request) = HttpRequest::parse_headers(header) {
        assert!(matches!(request.version.as_str(), "HTTP/1.0" | "HTTP/1.1"));
        assert!(request.path.starts_with('/'));
        assert!(!request.method.is_empty());
        assert!(request.header("Transfer-Encoding").is_none());
        if let Some(length) = request.header("Content-Length") {
            assert_eq!(length.parse::<usize>().ok(), request.content_length());
        }
        assert!(request
            .headers
            .iter()
            .all(|(name, value)| !name.is_empty() && !value.contains(['\r', '\n'])));
    }
});
