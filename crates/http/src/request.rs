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
            .is_some_and(|v| v.to_ascii_lowercase().split(',').any(|t| t.trim() == "close"))
    }

    pub fn conn_keep(&self) -> bool {
        self.header("Connection").is_some_and(|v| {
            v.to_ascii_lowercase()
                .split(',')
                .any(|t| t.trim() == "keep-alive")
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
        let mut parts = reqline.splitn(3, ' ');
        let method = parts.next().ok_or(ParseError::BadRequestLine)?.to_string();
        let target = parts.next().ok_or(ParseError::BadRequestLine)?.to_string();
        let version = parts.next().ok_or(ParseError::BadRequestLine)?.to_string();
        if method.is_empty() || target.is_empty() {
            return Err(ParseError::BadRequestLine);
        }
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
        };
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            headers.push((k.trim().to_string(), v.trim().to_string()));
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
