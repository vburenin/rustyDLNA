//! HTTP persist rule from `src/httpersist.h`.

/// HTTP/1.1 stays open unless the client sent `Connection: close`.
/// HTTP/1.0 stays open only with an explicit `Connection: keep-alive`.
/// `close` always wins if both tokens are present.
pub fn http_should_persist(httpver: Option<&str>, conn_close: bool, conn_keep: bool) -> bool {
    if httpver.is_none() || conn_close {
        return false;
    }
    match httpver {
        Some("HTTP/1.1") => true,
        Some("HTTP/1.0") => conn_keep,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_rules() {
        assert!(http_should_persist(Some("HTTP/1.1"), false, false));
        assert!(!http_should_persist(Some("HTTP/1.1"), true, true));
        assert!(!http_should_persist(Some("HTTP/1.0"), false, false));
        assert!(http_should_persist(Some("HTTP/1.0"), false, true));
        assert!(!http_should_persist(None, false, true));
    }
}
