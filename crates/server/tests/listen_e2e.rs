//! In-container listen tests. Bind only when RUSTY_DLNA_HTTP_PORT is set
//! (compose sets 18200 / 11900). Host `cargo test` skips sockets.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

fn env_ports() -> Option<(u16, u16)> {
    let http = std::env::var("RUSTY_DLNA_HTTP_PORT").ok()?.parse().ok()?;
    let ssdp = std::env::var("RUSTY_DLNA_SSDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(11900);
    Some((http, ssdp))
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn raw_http(port: u16, req: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(8))).ok();
    s.set_write_timeout(Some(Duration::from_secs(8))).ok();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
    let headers = String::from_utf8_lossy(&buf[..split]).into_owned();
    let body = if split + 4 <= buf.len() {
        buf[split + 4..].to_vec()
    } else {
        Vec::new()
    };
    (status, headers, body)
}

fn soap(port: u16, ua: &str, action: &str, inner: &str) -> (u16, String) {
    let body = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{inner}</s:Body></s:Envelope>"#
    );
    let req = format!(
        "POST /unused HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: {ua}\r\nSOAPAction: \"{action}\"\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (st, _, b) = raw_http(port, &req);
    (st, String::from_utf8_lossy(&b).into_owned())
}

fn wait_http(port: u16) {
    for _ in 0..80 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not bind 127.0.0.1:{port}");
}

fn ensure_fixtures() {
    let lib = workspace().join("testdata/library");
    rusty_dlna::ensure_pattern_fixture(&lib);
    let dvp7 = lib.join("video/dvp7.mkv");
    if !dvp7.exists() || dvp7.metadata().map(|m| m.len() < 200).unwrap_or(true) {
        if let Ok(st) = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                dvp7.to_str().unwrap(),
            ])
            .status()
        {
            if !st.success() {
                let _ = std::fs::write(&dvp7, b"tiny-placeholder");
            }
        } else {
            let _ = std::fs::write(&dvp7, b"tiny-placeholder");
        }
    }
}

fn spawn_bin(http: u16, ssdp: u16) -> Server {
    ensure_fixtures();
    let root = workspace();
    let bin = env!("CARGO_BIN_EXE_rusty-dlna");
    let child = Command::new(bin)
        .current_dir(&root)
        .arg("--config")
        .arg(root.join("testdata/rusty-dlna.test.toml"))
        .env("RUSTY_DLNA_HTTP_PORT", http.to_string())
        .env("RUSTY_DLNA_SSDP_PORT", ssdp.to_string())
        .env("RUST_LOG", "info")
        .spawn()
        .expect("spawn rusty-dlna");
    wait_http(http);
    Server(child)
}

fn one_run(http: u16, ssdp: u16) {
    let _srv = spawn_bin(http, ssdp);

    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: Kodi/21.0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "rootDesc {hdr}");
    let xml = String::from_utf8_lossy(&body);
    assert!(xml.contains("MediaServer:1"), "{xml}");
    assert!(hdr.contains("DLNADOC/1.50 UPnP/1.0 rustyDLNA/"), "{hdr}");

    let inner = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let (st, browse0) = soap(
        http,
        "Kodi/21.0 (Linux)",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner,
    );
    assert_eq!(st, 200);
    assert!(browse0.contains("&lt;DIDL-Lite"), "{browse0}");
    assert!(browse0.contains("64"), "{browse0}");
    assert!(browse0.contains("id=&quot;1&quot;") || browse0.contains("&quot;1&quot;"));
    assert!(browse0.contains("id=&quot;2&quot;") || browse0.contains("&quot;2&quot;"));

    let inner8 = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let items = {
        let mut last = String::new();
        for _ in 0..40 {
            let (_, body) = soap(
                http,
                "Kodi/21.0",
                "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
                inner8,
            );
            if body.contains("/MediaItems/") {
                last = body;
                break;
            }
            last = body;
            std::thread::sleep(Duration::from_millis(50));
        }
        last
    };
    assert!(items.contains("/MediaItems/"), "{items}");
    assert!(
        items.contains("1999-01-01") || items.contains("Z&lt;/dc:date"),
        "dc:date {items}"
    );

    // original GET + two ranges — pick the movie title's MediaItems id
    let movie_at = items.find("movie").expect("movie title");
    let after = &items[movie_at..];
    let url_start = after.find("/MediaItems/").expect("movie res");
    let rest = &after[url_start..];
    let end = rest.find("&lt;").or_else(|| rest.find('<')).unwrap_or(rest.len());
    let path = rest[..end].split(".mkv").next().unwrap();
    let id: i64 = path
        .trim_start_matches("/MediaItems/")
        .parse()
        .expect("id");
    let movie = workspace().join("testdata/library/video/movie.mkv");
    let expect = std::fs::read(&movie).unwrap();
    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: Kodi/21.0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200);
    assert!(hdr.to_ascii_lowercase().contains("accept-ranges: bytes"));
    assert!(hdr.contains("DLNA.ORG_OP=01"));
    assert!(hdr.contains("DLNA.ORG_CI=0"));
    assert!(hdr.to_ascii_lowercase().contains("connection: close"));
    assert_eq!(body, expect);

    let (st, _, b1) = raw_http(
        http,
        &format!("GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nRange: bytes=0-63\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 206);
    assert_eq!(b1, expect[0..64]);
    let (st, _, b2) = raw_http(
        http,
        &format!("GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nRange: bytes=64-127\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 206);
    assert_eq!(b2, expect[64..128]);

    // CrKey remap DIDL
    let (_, cr) = soap(
        http,
        "CrKey/1.54.384650 DLNADOC/1.50",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner8,
    );
    assert!(cr.contains("/Transcode/"), "CrKey DIDL missing remap {cr}");
    assert!(cr.contains("DLNA.ORG_CI=1"));

    if let Some(tpos) = cr.find("/Transcode/") {
        let rest = &cr[tpos + "/Transcode/".len()..];
        let tid: i64 = rest
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if tid > 0 && Command::new("ffmpeg").arg("-version").status().is_ok() {
            let (st, hdr, body) = raw_http(
                http,
                &format!("GET /Transcode/{tid}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: CrKey/1.54\r\nConnection: close\r\n\r\n"),
            );
            if st == 200 && !body.is_empty() {
                assert!(hdr.to_ascii_lowercase().contains("accept-ranges: bytes"));
                let mid = body.len() / 2;
                let end = (mid + 15).min(body.len() - 1);
                let (st2, _, slice) = raw_http(
                    http,
                    &format!("GET /Transcode/{tid}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: CrKey/1.54\r\nRange: bytes={mid}-{end}\r\nConnection: close\r\n\r\n"),
                );
                assert_eq!(st2, 206);
                assert_eq!(slice, body[mid..=end]);
            }
        }
    }

    // SSDP M-SEARCH
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let pkt = "M-SEARCH * HTTP/1.1\r\nHOST:127.0.0.1:11900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
    sock.send_to(pkt.as_bytes(), ("127.0.0.1", ssdp)).unwrap();
    let mut got = 0;
    let mut last = String::new();
    let mut buf = [0u8; 2048];
    while got < 6 {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                last = String::from_utf8_lossy(&buf[..n]).into_owned();
                got += 1;
            }
            Err(_) => break,
        }
    }
    assert_eq!(got, 6, "ssdp:all replies={got} last={last}");
    assert!(last.contains("LOCATION: http://"));
    assert!(last.contains("/rootDesc.xml"));
    assert!(last.contains("max-age=1800"));

    let bad = "M-SEARCH * HTTP/1.1\r\nMAN: ssdp:discover\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
    sock.send_to(bad.as_bytes(), ("127.0.0.1", ssdp)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let rejected = sock.recv_from(&mut buf).is_err();
    assert!(rejected, "bad MAN must not reply");
}

#[test]
fn container_listen_twice() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip listen e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    one_run(http, ssdp);
    one_run(http, ssdp);
}
