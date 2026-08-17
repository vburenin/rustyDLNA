//! In-memory GENA subscriber table and NOTIFY (`replica.md` §9).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_SUBSCRIBERS: usize = 500;
pub const DEFAULT_TIMEOUT_SECS: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventService {
    ContentDir,
    ConnMgr,
    Registrar,
}

#[derive(Clone, Debug)]
pub struct Subscriber {
    pub sid: String,
    pub callback: String,
    pub service: EventService,
    pub timeout_secs: u32,
    pub seq: u32,
    pub created: Instant,
}

#[derive(Clone, Debug)]
pub struct NotifyJob {
    pub sid: String,
    pub callback: String,
    pub seq: u32,
}

#[derive(Clone, Debug)]
pub struct CallbackUrl {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub path: String,
}

impl CallbackUrl {
    pub fn as_url(&self) -> String {
        if self.port == 80 {
            format!("http://{}{}", self.ip, self.path)
        } else {
            format!("http://{}:{}{}", self.ip, self.port, self.path)
        }
    }

    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.ip.to_string()
        } else {
            format!("{}:{}", self.ip, self.port)
        }
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.ip, self.port))
    }
}

#[derive(Debug, Default)]
pub struct EventHub {
    subs: Vec<Subscriber>,
}

impl EventHub {
    pub fn new() -> Self {
        Self { subs: Vec::new() }
    }

    pub fn gc(&mut self) {
        let now = Instant::now();
        self.subs.retain(|s| {
            now.duration_since(s.created).as_secs() < u64::from(s.timeout_secs.max(1))
        });
    }

    fn find_mut(&mut self, sid: &str) -> Option<&mut Subscriber> {
        let sid = sid.trim();
        self.subs.iter_mut().find(|s| s.sid == sid)
    }

    pub fn subscribe_new(
        &mut self,
        sid: String,
        callback: String,
        service: EventService,
        timeout_secs: u32,
    ) -> Result<NotifyJob, u16> {
        self.gc();
        if self.subs.len() >= MAX_SUBSCRIBERS {
            return Err(412);
        }
        let job = NotifyJob {
            sid: sid.clone(),
            callback: callback.clone(),
            seq: 0,
        };
        self.subs.push(Subscriber {
            sid,
            callback,
            service,
            timeout_secs,
            seq: 1,
            created: Instant::now(),
        });
        Ok(job)
    }

    pub fn renew(&mut self, sid: &str) -> Result<u32, u16> {
        self.gc();
        let sub = self.find_mut(sid).ok_or(412u16)?;
        sub.timeout_secs = DEFAULT_TIMEOUT_SECS;
        sub.created = Instant::now();
        Ok(DEFAULT_TIMEOUT_SECS)
    }

    pub fn unsubscribe(&mut self, sid: &str) -> Result<(), u16> {
        self.gc();
        let sid = sid.trim();
        let before = self.subs.len();
        self.subs.retain(|s| s.sid != sid);
        if self.subs.len() == before {
            Err(412)
        } else {
            Ok(())
        }
    }

    pub fn take_content_dir_notifies(&mut self) -> Vec<NotifyJob> {
        self.gc();
        let mut jobs = Vec::new();
        for s in &mut self.subs {
            if s.service != EventService::ContentDir {
                continue;
            }
            let seq = s.seq;
            s.seq = seq.wrapping_add(1);
            if s.seq == 0 {
                s.seq = 1;
            }
            jobs.push(NotifyJob {
                sid: s.sid.clone(),
                callback: s.callback.clone(),
                seq,
            });
        }
        jobs
    }
}

pub fn service_from_path(path: &str) -> Option<EventService> {
    match path {
        rusty_dlna_protocol::paths::CONTENTDIRECTORY_EVENTURL => Some(EventService::ContentDir),
        rusty_dlna_protocol::paths::CONNECTIONMGR_EVENTURL => Some(EventService::ConnMgr),
        rusty_dlna_protocol::paths::X_MS_MEDIARECEIVERREGISTRAR_EVENTURL => {
            Some(EventService::Registrar)
        }
        _ => None,
    }
}

pub fn parse_callback(header: &str) -> Option<CallbackUrl> {
    let start = header.find('<')?;
    let rest = &header[start + 1..];
    let end = rest.find('>')?;
    parse_http_callback(rest[..end].trim())
}

fn parse_http_callback(url: &str) -> Option<CallbackUrl> {
    let rest = url.strip_prefix("http://")?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if auth.is_empty() {
        return None;
    }
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p))
            if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) =>
        {
            (h, p.parse().ok()?)
        }
        _ => (auth, 80u16),
    };
    if port == 0 {
        return None;
    }
    let ip: Ipv4Addr = host.parse().ok()?;
    let path = sanitize_callback_path(path)?;
    Some(CallbackUrl { ip, port, path })
}

/// Origin-form path only: leading `/`, printable ASCII, no CTL / space /
/// backslash. Stops `NOTIFY {path}` header injection via a lone LF in Callback.
fn sanitize_callback_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("/".into());
    }
    if !path.starts_with('/') || path.len() > 1024 {
        return None;
    }
    if !path
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b'\\')
    {
        return None;
    }
    Some(path.to_string())
}

pub fn peer_ipv4(peer: SocketAddr) -> Option<Ipv4Addr> {
    match peer {
        SocketAddr::V4(v) => Some(*v.ip()),
        SocketAddr::V6(v) => v.ip().to_ipv4_mapped().or(v.ip().to_ipv4()),
    }
}

pub fn propertyset(service: EventService, update_id: u32) -> String {
    match service {
        EventService::ContentDir => format!(
            "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\r\n  <e:property><SystemUpdateID>{update_id}</SystemUpdateID></e:property>\r\n</e:propertyset>"
        ),
        EventService::ConnMgr => {
            "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:schemas-upnp-org:service:ConnectionManager:1\">\r\n  <e:property><SourceProtocolInfo>http-get:*:*:*</SourceProtocolInfo></e:property>\r\n  <e:property><SinkProtocolInfo></SinkProtocolInfo></e:property>\r\n  <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>\r\n</e:propertyset>".into()
        }
        EventService::Registrar => {
            "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1\">\r\n  <e:property><AuthorizationGrantedUpdateID>1</AuthorizationGrantedUpdateID></e:property>\r\n</e:propertyset>".into()
        }
    }
}

pub fn send_notify(job: &NotifyJob, body: &str) -> bool {
    let Some(cb) = parse_http_callback(&job.callback) else {
        return false;
    };
    let host = cb.host_header();
    let wire = format!(
        "NOTIFY {path} HTTP/1.1\r\n\
Host: {host}\r\n\
Content-Type: text/xml; charset=\"utf-8\"\r\n\
Content-Length: {n}\r\n\
NT: upnp:event\r\n\
NTS: upnp:propchange\r\n\
SID: {sid}\r\n\
SEQ: {seq}\r\n\
Connection: close\r\n\
Cache-Control: no-cache\r\n\
\r\n\
{body}",
        path = cb.path,
        n = body.len(),
        sid = job.sid,
        seq = job.seq,
    );
    let Ok(mut sock) = TcpStream::connect_timeout(&cb.socket_addr(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    if sock.write_all(wire.as_bytes()).is_err() {
        return false;
    }
    let _ = sock.shutdown(std::net::Shutdown::Write);
    let mut sink = [0u8; 128];
    let _ = sock.read(&mut sink);
    true
}

pub fn spawn_notify(job: NotifyJob, body: String) {
    let _ = std::thread::Builder::new()
        .name("gena-notify".into())
        .spawn(move || {
            let _ = send_notify(&job, &body);
        });
}

pub fn notify_content_dir(hub: &Mutex<EventHub>, update_id: u32) {
    let jobs = match hub.lock() {
        Ok(mut h) => h.take_content_dir_notifies(),
        Err(_) => return,
    };
    let body = propertyset(EventService::ContentDir, update_id);
    for job in jobs {
        spawn_notify(job, body.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_accepts_plain_ipv4() {
        let cb = parse_callback("<http://192.0.2.50:1234/evt>").expect("ok");
        assert_eq!(cb.ip, Ipv4Addr::new(192, 0, 2, 50));
        assert_eq!(cb.port, 1234);
        assert_eq!(cb.path, "/evt");
    }

    #[test]
    fn parse_callback_rejects_header_injection() {
        assert!(parse_callback("<http://192.0.2.50:1234/evt\nHost: pwn>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/evt\r\nX: 1>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/foo bar>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/foo\\bar>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234>").is_some());
    }
}
