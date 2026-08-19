//! HTTP/1.x request line + headers. Body is left to the caller.

#[derive(Clone, Debug, Default)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub path: String,
    pub query: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Incomplete,
    BadRequestLine,
    UnsupportedVersion,
    MalformedHeader,
    ObsoleteFold,
    InvalidHeaderName,
    InvalidHeaderValue,
    InvalidContentLength,
    ConflictingContentLength,
    TransferEncodingWithContentLength,
    UnsupportedTransferEncoding,
    DuplicateHost,
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
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
        })
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_length(&self) -> Option<usize> {
        self.header("Content-Length")?.parse().ok()
    }

    pub fn conn_close(&self) -> bool {
        self.header("Connection")
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
    }

    pub fn conn_keep(&self) -> bool {
        self.header("Connection").is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("keep-alive"))
        })
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.header("User-Agent")
    }

    /// Parse one request from a complete header block (up to `\r\n\r\n`).
    pub fn parse_headers(raw: &str) -> Result<Self, ParseError> {
        let (head, _) = raw.split_once("\r\n\r\n").ok_or(ParseError::Incomplete)?;
        let mut lines = head.split("\r\n");
        let reqline = lines.next().ok_or(ParseError::BadRequestLine)?;
        let mut parts = reqline.split(' ');
        let method = parts.next().ok_or(ParseError::BadRequestLine)?;
        let target = parts.next().ok_or(ParseError::BadRequestLine)?;
        let version = parts.next().ok_or(ParseError::BadRequestLine)?;
        if parts.next().is_some()
            || !is_token(method)
            || target.is_empty()
            || !target.starts_with('/')
            || target.bytes().any(|byte| byte <= b' ' || byte == 0x7f)
        {
            return Err(ParseError::BadRequestLine);
        }
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
            return Err(ParseError::UnsupportedVersion);
        }
        let method = method.to_string();
        let target = target.to_string();
        let version = version.to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
        };
        let mut headers = Vec::new();
        let mut content_length = None;
        let mut transfer_encoding = false;
        let mut host_seen = false;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if line.starts_with([' ', '\t']) {
                return Err(ParseError::ObsoleteFold);
            }
            let Some((name, raw_value)) = line.split_once(':') else {
                return Err(ParseError::MalformedHeader);
            };
            if !is_token(name) {
                return Err(ParseError::InvalidHeaderName);
            }
            let value = trim_ows(raw_value);
            if value
                .bytes()
                .any(|byte| byte < b' ' && byte != b'\t' || byte == 0x7f)
            {
                return Err(ParseError::InvalidHeaderValue);
            }
            if name.eq_ignore_ascii_case("Content-Length") {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ParseError::InvalidContentLength);
                }
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| ParseError::InvalidContentLength)?;
                if content_length.is_some_and(|previous| previous != parsed) {
                    return Err(ParseError::ConflictingContentLength);
                }
                if content_length.is_none() {
                    content_length = Some(parsed);
                    headers.push((name.to_string(), value.to_string()));
                }
                continue;
            }
            if name.eq_ignore_ascii_case("Transfer-Encoding") {
                transfer_encoding = true;
            }
            if name.eq_ignore_ascii_case("Host") {
                if host_seen {
                    return Err(ParseError::DuplicateHost);
                }
                host_seen = true;
            }
            if let Some((_, existing)) = headers
                .iter_mut()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
            {
                existing.push_str(", ");
                existing.push_str(value);
            } else {
                headers.push((name.to_string(), value.to_string()));
            }
        }
        if transfer_encoding && content_length.is_some() {
            return Err(ParseError::TransferEncodingWithContentLength);
        }
        if transfer_encoding {
            return Err(ParseError::UnsupportedTransferEncoding);
        }
        Ok(Self {
            method,
            target,
            path,
            query,
            version,
            headers,
            body: Vec::new(),
        })
    }
}

pub fn header_block_complete(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(headers: &str) -> Result<HttpRequest, ParseError> {
        HttpRequest::parse_headers(headers)
    }

    #[test]
    fn accepts_strict_http_10_and_11_requests() {
        let request =
            parse("POST /ctl?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 7\r\n\r\n")
                .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/ctl");
        assert_eq!(request.query, "x=1");
        assert_eq!(request.content_length(), Some(7));
        assert!(parse("GET / HTTP/1.0\r\n\r\n").is_ok());
    }

    #[test]
    fn rejects_malformed_request_lines_and_headers() {
        for (raw, expected) in [
            ("GET  / HTTP/1.1\r\n\r\n", ParseError::BadRequestLine),
            ("GET / HTTP/2\r\n\r\n", ParseError::UnsupportedVersion),
            ("GE(T / HTTP/1.1\r\n\r\n", ParseError::BadRequestLine),
            ("GET relative HTTP/1.1\r\n\r\n", ParseError::BadRequestLine),
            ("GET / HTTP/1.1 extra\r\n\r\n", ParseError::BadRequestLine),
            (
                "GET / HTTP/1.1\r\nBroken\r\n\r\n",
                ParseError::MalformedHeader,
            ),
            (
                "GET / HTTP/1.1\r\n Host: x\r\n\r\n",
                ParseError::ObsoleteFold,
            ),
            (
                "GET / HTTP/1.1\r\nHost : x\r\n\r\n",
                ParseError::InvalidHeaderName,
            ),
            (
                "GET / HTTP/1.1\r\nBad(Name: x\r\n\r\n",
                ParseError::InvalidHeaderName,
            ),
            (
                "GET / HTTP/1.1\r\nX-Test: ok\u{7f}\r\n\r\n",
                ParseError::InvalidHeaderValue,
            ),
        ] {
            assert_eq!(parse(raw).unwrap_err(), expected, "{raw:?}");
        }
    }

    #[test]
    fn rejects_length_and_transfer_encoding_smuggling() {
        assert_eq!(
            parse("POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n").unwrap_err(),
            ParseError::InvalidContentLength
        );
        assert_eq!(
            parse("POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n").unwrap_err(),
            ParseError::ConflictingContentLength
        );
        let same =
            parse("POST / HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n").unwrap();
        assert_eq!(same.content_length(), Some(2));
        assert_eq!(
            parse("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\n")
                .unwrap_err(),
            ParseError::TransferEncodingWithContentLength
        );
        assert_eq!(
            parse("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err(),
            ParseError::UnsupportedTransferEncoding
        );
        assert_eq!(
            parse("GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n").unwrap_err(),
            ParseError::DuplicateHost
        );
    }

    #[test]
    fn combines_safe_duplicate_fields() {
        let request =
            parse("GET / HTTP/1.1\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n").unwrap();
        assert!(request.conn_close());
        assert!(request.conn_keep());
    }

    #[test]
    fn smuggling_edge_cases_and_first_pipeline_boundary_are_unambiguous() {
        for raw in [
            "POST / HTTP/1.1\r\nContent-Length: 1, 1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length:\t1 2\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: identity\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n\r\n",
            "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: a\r\nhOsT: b\r\n\r\n",
            "GET / HTTP/1.1\nHost: a\n\n",
            "GET /nul\0tail HTTP/1.1\r\n\r\n",
        ] {
            assert!(parse(raw).is_err(), "accepted ambiguous request: {raw:?}");
        }

        let first = b"POST /one HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc";
        let second = b"GET /two HTTP/1.1\r\nConnection: close\r\n\r\n";
        let mut pipeline = first.to_vec();
        pipeline.extend_from_slice(second);
        let header_end = header_block_complete(&pipeline).unwrap();
        assert_eq!(header_end, first.len() - 3);
        let parsed =
            HttpRequest::parse_headers(std::str::from_utf8(&pipeline[..header_end]).unwrap())
                .unwrap();
        assert_eq!(parsed.content_length(), Some(3));
        assert_eq!(&pipeline[header_end..header_end + 3], b"abc");
        assert_eq!(&pipeline[header_end + 3..], second);
    }
}
