//! Live listen tests against a spawned `rusty-dlna` binary.
//!
//! Host / this environment:
//! ```
//! RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900 \
//!   cargo test -p rusty-dlna --test listen_e2e -- --test-threads=1
//! ```
//! Tests offset from those ports so they can run together:
//! `one_run` +0, byebye +1, series/remux +2, body-cap +3, dialect +4,
//! remaining +5, kodi-platinum +6. Never bind live 8200/1900 (`isolation`).
//!
//! Unset env → skip for ordinary unit runs. `RUSTY_DLNA_REQUIRE_E2E=1`
//! converts a missing/invalid environment into a hard failure for CI.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

fn env_ports() -> Option<(u16, u16)> {
    let required = std::env::var("RUSTY_DLNA_REQUIRE_E2E").as_deref() == Ok("1");
    let http = match std::env::var("RUSTY_DLNA_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        Some(port) => port,
        None if required => {
            panic!("RUSTY_DLNA_REQUIRE_E2E=1 requires a valid RUSTY_DLNA_HTTP_PORT")
        }
        None => return None,
    };
    let ssdp = std::env::var("RUSTY_DLNA_SSDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(11900);
    Some((http, ssdp))
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Server {
    child: Child,
    temp_dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // SIGTERM so `serve` can send SSDP byebye. SIGKILL skips that path.
        #[cfg(unix)]
        unsafe {
            // SAFETY: `Child::id` is the live process we spawned, and SIGTERM
            // does not dereference memory or outlive this cleanup operation.
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
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
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
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

fn browse_inner(oid: &str, flag: &str) -> String {
    format!(
        r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>{oid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#
    )
}

fn wait_media_items(http: u16, ua: &str, oid: &str) -> String {
    let inner = browse_inner(oid, "BrowseDirectChildren");
    let mut last = String::new();
    for _ in 0..40 {
        let (_, body) = soap(
            http,
            ua,
            "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
            &inner,
        );
        if body.contains("/MediaItems/") {
            return body;
        }
        last = body;
        std::thread::sleep(Duration::from_millis(50));
    }
    last
}

/// Movie title's `/MediaItems/{id}` — same parse as `one_run`.
fn movie_media_id(items: &str) -> i64 {
    let movie_at = items
        .find("Fixture Movie")
        .or_else(|| items.find("movie"))
        .expect("movie title");
    let after = &items[movie_at..];
    let url_start = after.find("/MediaItems/").expect("movie res");
    let rest = &after[url_start..];
    let end = rest
        .find("&lt;")
        .or_else(|| rest.find('<'))
        .unwrap_or(rest.len());
    let path = rest[..end].split(".mkv").next().unwrap();
    path.trim_start_matches("/MediaItems/").parse().expect("id")
}

fn feature_has_id(xml: &str, id: &str) -> bool {
    xml.contains(&format!("id=&quot;{id}&quot;")) || xml.contains(&format!("id=\"{id}\""))
}

fn hdr_has(hdr: &str, needle: &str) -> bool {
    hdr.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn status_row_count(body: &str, label: &str) -> Option<u32> {
    let marker = format!("<tr><td>{label}</td><td>");
    let rest = body.split(&marker).nth(1)?;
    rest.split("</td>").next()?.parse().ok()
}

fn parse_album_art_url(didl: &str) -> Option<(i64, i64)> {
    let idx = didl.find("/AlbumArt/")?;
    let rest = &didl[idx + "/AlbumArt/".len()..];
    let art: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let art_id = art.parse().ok()?;
    let after = rest.get(art.len()..)?;
    let after = after.strip_prefix('-')?;
    let det: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    let detail_id = det.parse().ok()?;
    Some((art_id, detail_id))
}

fn ensure_fixtures() {
    let lib = workspace().join("testdata/library");
    rusty_dlna::ensure_pattern_fixture(&lib);
    rusty_dlna::ensure_show_fixture(&lib);
    let music = lib.join("music");
    let _ = std::fs::create_dir_all(&music);
    let song = music.join("song.mp3");
    if !song.exists() {
        let _ = std::fs::write(&song, b"ID3fake");
    }
    let song_nfo = music.join("song.nfo");
    if !song_nfo.exists() {
        let _ = std::fs::write(
            &song_nfo,
            "<musicvideo><title>Fixture Track</title><genre>Jazz</genre><studio>Fixture Band</studio></musicvideo>\n",
        );
    }
    let pictures = lib.join("pictures");
    let _ = std::fs::create_dir_all(&pictures);
    let shot = pictures.join("shot.jpg");
    if !shot.exists() {
        let _ = std::fs::write(
            &shot,
            [
                0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xd9,
            ],
        );
    }
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
    spawn_bin_with_sink(http, ssdp, None)
}

fn spawn_bin_with_sink(http: u16, ssdp: u16, sink: Option<String>) -> Server {
    ensure_fixtures();
    let root = workspace();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!(
        "rusty-dlna-e2e-{}-{http}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp_dir).expect("create E2E temporary directory");
    let config_path = temp_dir.join("rusty-dlna.toml");
    let media_dir = root.join("testdata/library");
    let config = format!(
        r#"friendly_name = "rustyDLNA-test"
media_dir = ["{}"]
exclude_dir = ["exclude_me"]
cache_dir = "cache"
db_dir = "database"
advertise_ip = "127.0.0.1"
uuid = "uuid:00000000-0000-4000-8000-000000000001"
notify_interval = 895

[transcode]
enable = true
encoder = "libx264"
max_jobs = 1

[[remap]]
name = "crkey-dvp7"
client = "CrKey"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
        media_dir.display()
    );
    std::fs::write(&config_path, config).expect("write E2E configuration");
    let bin = env!("CARGO_BIN_EXE_rusty-dlna");
    let mut cmd = Command::new(bin);
    cmd.current_dir(&root)
        .arg("--config")
        .arg(config_path)
        .env("RUSTY_DLNA_HTTP_PORT", http.to_string())
        .env("RUSTY_DLNA_SSDP_PORT", ssdp.to_string())
        .env(
            "RUST_LOG",
            std::env::var("RUSTY_DLNA_E2E_LOG").unwrap_or_else(|_| "off".into()),
        );
    if let Some(s) = sink {
        cmd.env("RUSTY_DLNA_SSDP_SINK", s);
    }
    let child = cmd.spawn().expect("spawn rusty-dlna");
    let server = Server { child, temp_dir };
    wait_http(http);
    server
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
    assert!(
        browse0.contains("id=\"1\"")
            || browse0.contains("id=&quot;1&quot;")
            || browse0.contains("&quot;1&quot;"),
        "{browse0}"
    );
    assert!(
        browse0.contains("id=\"2\"")
            || browse0.contains("id=&quot;2&quot;")
            || browse0.contains("&quot;2&quot;"),
        "{browse0}"
    );

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
        items.contains("Fixture Movie"),
        "NFO title in Browse: {items}"
    );
    assert!(
        items.contains("1999-01-01") || items.contains("Z&lt;/dc:date"),
        "dc:date {items}"
    );
    assert!(
        items.contains("/AlbumArt/") && items.contains("JPEG_TN"),
        "album art DIDL: {items}"
    );

    // original GET + two ranges — pick the movie title's MediaItems id
    let movie_at = items
        .find("Fixture Movie")
        .or_else(|| items.find("movie"))
        .expect("movie title");
    let after = &items[movie_at..];
    let url_start = after.find("/MediaItems/").expect("movie res");
    let rest = &after[url_start..];
    let end = rest
        .find("&lt;")
        .or_else(|| rest.find('<'))
        .unwrap_or(rest.len());
    let path = rest[..end].split(".mkv").next().unwrap();
    let id: i64 = path.trim_start_matches("/MediaItems/").parse().expect("id");
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

    // Phase 10 /AlbumArt/ + Phase 16 /status on the live test listener
    let (art_id, art_detail) = parse_album_art_url(&items).expect("parse /AlbumArt/");
    assert!(art_id > 0, "album art id");
    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /AlbumArt/{art_id}-{art_detail}.jpg HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "album art GET {hdr}");
    assert!(hdr.to_ascii_lowercase().contains("image/jpeg"));
    assert!(body.len() >= 3 && body[0] == 0xff && body[1] == 0xd8);
    let (st, _hdr, status_body) = raw_http(
        http,
        &format!("GET /status HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200);
    let status_txt = String::from_utf8_lossy(&status_body);
    assert!(status_txt.contains("Video"), "{status_txt}");
    let video_n = status_row_count(&status_txt, "Video").expect("Video row");
    assert!(video_n >= 1, "status video count {video_n}: {status_txt}");

    // CrKey remap DIDL
    let (_, cr) = soap(
        http,
        "CrKey/1.54.384650 DLNADOC/1.50",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner8,
    );
    assert!(cr.contains("/Transcode/"), "CrKey DIDL missing remap {cr}");
    assert!(cr.contains("DLNA.ORG_CI=1"));

    let tpos = cr.find("/Transcode/").expect("CrKey DIDL missing remap");
    let rest = &cr[tpos + "/Transcode/".len()..];
    let tid: i64 = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("transcode id");
    assert!(tid > 0, "transcode id");
    let (st, hdr, _body) = raw_http(
        http,
        &format!("GET /Transcode/{tid}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: CrKey/1.54\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "remux GET {st} {hdr}");
    assert!(
        hdr.contains("DLNA.ORG_CI=1") || hdr.to_ascii_lowercase().contains("dlna.org_ci=1"),
        "remux must set CI=1: {hdr}"
    );
    assert!(
        hdr.to_ascii_lowercase().contains("video/mp4"),
        "remux Content-Type: {hdr}"
    );
    assert!(
        hdr.contains("DLNA.ORG_OP=00") || hdr.contains("DLNA.ORG_OP=01"),
        "remux OP: {hdr}"
    );

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

    sock.set_read_timeout(Some(Duration::from_millis(50))).ok();
    while sock.recv_from(&mut buf).is_ok() {}
    let bad = "M-SEARCH * HTTP/1.1\r\nMAN: ssdp:discover\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
    sock.send_to(bad.as_bytes(), ("127.0.0.1", ssdp)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let rejected = sock.recv_from(&mut buf).is_err();
    assert!(rejected, "bad MAN must not reply");
}

#[test]
fn request_body_cap_and_soap_host() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip body-cap e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    let http = http.saturating_add(3);
    let ssdp = ssdp.saturating_add(3);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let _srv = spawn_bin(http, ssdp);

    let over = 256 * 1024 * 1024 + 1;
    let (st, hdr, _) = raw_http(
        http,
        &format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nContent-Length: {over}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(st, 413, "oversized Content-Length must 413: {hdr}");

    let (st, hdr, _) = raw_http(
        http,
        "POST /ctl/ContentDir HTTP/1.1\r\nHost: attacker.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(st, 400, "SOAP hostname Host must 400: {hdr}");
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

#[test]
fn ssdp_byebye_on_drop() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip listen e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    // Distinct ports so this can run beside `container_listen_twice`.
    let http = http.saturating_add(1);
    let ssdp = ssdp.saturating_add(1);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    sink.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let sink_addr = sink.local_addr().unwrap();
    let srv = spawn_bin_with_sink(http, ssdp, Some(sink_addr.to_string()));
    drop(srv);
    let mut buf = [0u8; 2048];
    let mut got = 0;
    let mut last = String::new();
    let mut saw_byebye = false;
    let mut saw_location = false;
    while got < 12 {
        match sink.recv_from(&mut buf) {
            Ok((n, _)) => {
                last = String::from_utf8_lossy(&buf[..n]).into_owned();
                if last.contains("NTS:ssdp:byebye") {
                    saw_byebye = true;
                }
                if last.contains("LOCATION") {
                    saw_location = true;
                }
                got += 1;
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_byebye && got >= 6,
        "byebye on drop: got={got} last={last}"
    );
    assert!(!saw_location, "byebye must omit LOCATION: {last}");
}

#[test]
fn series_genre_and_remux_e2e() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip series/remux e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    let http = http.saturating_add(2);
    let ssdp = ssdp.saturating_add(2);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let _srv = spawn_bin(http, ssdp);

    let inner_v = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let (st, video) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner_v,
    );
    assert_eq!(st, 200);
    assert!(video.contains("Series"), "{video}");
    assert!(video.contains("Genre"), "{video}");

    let mut series = String::new();
    for _ in 0..40 {
        let inner = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$E</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
        let (_, body) = soap(
            http,
            "Kodi/21.0",
            "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
            inner,
        );
        if body.contains("The Show") {
            series = body;
            break;
        }
        series = body;
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(series.contains("The Show"), "Series Browse: {series}");

    let inner_g = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$9</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let (st, genres) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner_g,
    );
    assert_eq!(st, 200);
    assert!(
        genres.contains("Drama") || genres.contains("Crime"),
        "Genre Browse: {genres}"
    );

    let inner8 = r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let (_, cr) = soap(
        http,
        "CrKey/1.54.384650 DLNADOC/1.50",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        inner8,
    );
    assert!(cr.contains("/Transcode/"), "CrKey DIDL {cr}");
    let tpos = cr.find("/Transcode/").expect("CrKey DIDL missing remap");
    let rest = &cr[tpos + "/Transcode/".len()..];
    let tid: i64 = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("transcode id");
    assert!(tid > 0, "transcode id");
    let (st, hdr, _body) = raw_http(
        http,
        &format!("GET /Transcode/{tid}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: CrKey/1.54\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "transcode status {st} {hdr}");
    assert!(
        hdr.contains("DLNA.ORG_CI=1") || hdr.to_ascii_lowercase().contains("dlna.org_ci=1"),
        "{hdr}"
    );
    if hdr.contains("DLNA.ORG_OP=01") {
        assert!(hdr.to_ascii_lowercase().contains("accept-ranges"));
        let (st2, hdr2, body2) = raw_http(
            http,
            &format!("GET /Transcode/{tid}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: CrKey/1.54\r\nRange: bytes=0-15\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st2, 206, "{hdr2}");
        assert_eq!(body2.len(), 16);
    }
}

#[test]
fn replica_dialect_e2e() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip dialect e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    // +4: beside twice (base), byebye (+1), series/remux (+2), body-cap (+3).
    let http = http.saturating_add(4);
    let ssdp = ssdp.saturating_add(4);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let _srv = spawn_bin(http, ssdp);

    // 1. Kodi rootDesc: MediaServer + rustyDLNA Server token
    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: Kodi/21.0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "kodi rootDesc {hdr}");
    let kodi_desc = String::from_utf8_lossy(&body);
    assert!(kodi_desc.contains("MediaServer:1"), "{kodi_desc}");
    assert!(
        hdr_has(&hdr, "rustyDLNA/") || hdr.contains("rustyDLNA"),
        "Server token: {hdr}"
    );

    // 2. Xbox rootDesc: modelNumber 1 and friendlyName contains `: 1`
    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: Xbox/360\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "xbox rootDesc {hdr}");
    let xbox_desc = String::from_utf8_lossy(&body);
    assert!(
        xbox_desc.contains("<modelNumber>1</modelNumber>"),
        "{xbox_desc}"
    );
    let friendly = xbox_desc
        .split("<friendlyName>")
        .nth(1)
        .and_then(|s| s.split("</friendlyName>").next())
        .unwrap_or("");
    assert!(
        friendly.contains(": 1"),
        "friendlyName must contain : 1 inside the tag: {xbox_desc}"
    );

    // 3. Samsung TV rootDesc: ProductCap / X_ProductCap (URL slots replaced)
    let (st, hdr, body) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "samsung rootDesc {hdr}");
    let tv_desc = String::from_utf8_lossy(&body);
    const DCM10_CAP: &str = "smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec";
    for tag in ["ProductCap", "X_ProductCap"] {
        let open = format!("<{tag}>");
        let open_sec = format!("<sec:{tag}>");
        let close = format!("</{tag}>");
        let close_sec = format!("</sec:{tag}>");
        let inner = tv_desc
            .split(&open_sec)
            .nth(1)
            .or_else(|| tv_desc.split(&open).nth(1))
            .and_then(|s| {
                s.split(&close_sec)
                    .next()
                    .or_else(|| s.split(&close).next())
            })
            .unwrap_or("");
        assert!(
            inner.contains(DCM10_CAP),
            "{tag} must contain {DCM10_CAP}: {tv_desc}"
        );
    }

    // 4. BrowseMetadata ObjectID=0: upnp:searchClass audio/image/video
    let (st, meta) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("0", "BrowseMetadata"),
    );
    assert_eq!(st, 200, "BrowseMetadata 0: {meta}");
    assert!(meta.contains("searchClass"), "{meta}");
    assert!(meta.contains("audioItem"), "{meta}");
    assert!(meta.contains("imageItem"), "{meta}");
    assert!(meta.contains("videoItem"), "{meta}");

    // 5. Browse 2: av:mediaClass V + video tree children
    let (st, video) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("2", "BrowseDirectChildren"),
    );
    assert_eq!(st, 200, "Browse 2: {video}");
    assert!(video.contains("av:mediaClass"), "mediaClass tag: {video}");
    assert!(
        video.contains(">V<")
            || video.contains(">V&lt;")
            || video.contains("&gt;V&lt;")
            || video.contains("&gt;V<"),
        "av:mediaClass must be V: {video}"
    );
    for title in [
        "Actor",
        "All Video",
        "Folders",
        "Genre",
        "Recently Added",
        "Series",
    ] {
        assert!(video.contains(title), "Browse 2 missing {title}: {video}");
    }

    // 6. Browse 8 Xbox/ UA == Browse 2$8 video-all items (PFS magic id)
    let items = wait_media_items(http, "Kodi/21.0", "2$8");
    assert!(items.contains("/MediaItems/"), "Browse 2$8: {items}");
    let id = movie_media_id(&items);
    let (st, xbox8) = soap(
        http,
        "Xbox/360",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("8", "BrowseDirectChildren"),
    );
    assert_eq!(st, 200, "Browse 8 Xbox: {xbox8}");
    assert!(
        xbox8.contains(&format!("/MediaItems/{id}")),
        "PFS Browse 8 must list same video-all id {id} as 2$8: {xbox8}"
    );
    assert!(
        xbox8.contains("Fixture Movie"),
        "Xbox Browse 8 missing Fixture Movie: {xbox8}"
    );

    // 7. X_GetFeatureList SEC_HHP_[TV] — ids A/V/I
    let (st, tv_fl) = soap(
        http,
        "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#X_GetFeatureList",
        r#"<u:X_GetFeatureList xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"></u:X_GetFeatureList>"#,
    );
    assert_eq!(st, 200, "TV FeatureList: {tv_fl}");
    assert!(feature_has_id(&tv_fl, "A"), "{tv_fl}");
    assert!(feature_has_id(&tv_fl, "V"), "{tv_fl}");
    assert!(feature_has_id(&tv_fl, "I"), "{tv_fl}");

    // 8. X_GetFeatureList SEC_HHP_[PC] — ids 1/2/3, not A/V/I
    let (st, pc_fl) = soap(
        http,
        "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#X_GetFeatureList",
        r#"<u:X_GetFeatureList xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"></u:X_GetFeatureList>"#,
    );
    assert_eq!(st, 200, "PC FeatureList: {pc_fl}");
    assert!(feature_has_id(&pc_fl, "1"), "{pc_fl}");
    assert!(feature_has_id(&pc_fl, "2"), "{pc_fl}");
    assert!(feature_has_id(&pc_fl, "3"), "{pc_fl}");
    assert!(!feature_has_id(&pc_fl, "A"), "PC must not use A: {pc_fl}");
    assert!(!feature_has_id(&pc_fl, "V"), "PC must not use V: {pc_fl}");
    assert!(!feature_has_id(&pc_fl, "I"), "PC must not use I: {pc_fl}");

    // 9. QueryStateVariable ConnectionStatus / missing / unknown
    let (st, qsv) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
        r#"<u:QueryStateVariable xmlns:u="urn:schemas-upnp-org:control-1-0"><varName>ConnectionStatus</varName></u:QueryStateVariable>"#,
    );
    assert_eq!(st, 200, "{qsv}");
    assert!(qsv.contains("<return>Connected</return>"), "{qsv}");
    let (st, qsv402) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
        r#"<u:QueryStateVariable xmlns:u="urn:schemas-upnp-org:control-1-0"></u:QueryStateVariable>"#,
    );
    assert_eq!(st, 500, "missing varName: {qsv402}");
    assert!(qsv402.contains("<errorCode>402</errorCode>"), "{qsv402}");
    let (st, qsv404) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:control-1-0#QueryStateVariable",
        r#"<u:QueryStateVariable xmlns:u="urn:schemas-upnp-org:control-1-0"><varName>NoSuchVar</varName></u:QueryStateVariable>"#,
    );
    assert_eq!(st, 500, "unknown var: {qsv404}");
    assert!(qsv404.contains("<errorCode>404</errorCode>"), "{qsv404}");

    // 10. GetProtocolInfo Source includes JPEG_TN and video/x-matroska
    let (st, proto) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo",
        r#"<u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"></u:GetProtocolInfo>"#,
    );
    assert_eq!(st, 200, "{proto}");
    assert!(proto.contains("JPEG_TN"), "{proto}");
    assert!(proto.contains("video/x-matroska"), "{proto}");

    // 11. X_SetBookmark no-such-object → HTTP 500 + 701
    let (st, bm) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#X_SetBookmark",
        r#"<u:X_SetBookmark xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>no-such-object</ObjectID><PosSecond>1</PosSecond></u:X_SetBookmark>"#,
    );
    assert_eq!(st, 500, "SetBookmark missing object: {bm}");
    assert!(bm.contains("<errorCode>701</errorCode>"), "{bm}");

    // 11b. Samsung Q CONVERT_MS: SOAP ms in, DIDL lastPlaybackPosition + BM= ms out.
    // `{id}` is DETAILS.ID from the movie /MediaItems/ URL; BrowseMetadata accepts it.
    let q = "SEC_HHP_[TV] Samsung Q";
    let (st, setq) = soap(
        http,
        q,
        "urn:schemas-upnp-org:service:ContentDirectory:1#X_SetBookmark",
        &format!(
            r#"<u:X_SetBookmark xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>{id}</ObjectID><PosSecond>120000</PosSecond></u:X_SetBookmark>"#
        ),
    );
    assert_eq!(st, 200, "Samsung Q SetBookmark: {setq}");
    assert!(setq.contains("X_SetBookmarkResponse"), "{setq}");
    let (st, qmeta) = soap(
        http,
        q,
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner(&id.to_string(), "BrowseMetadata"),
    );
    assert_eq!(st, 200, "Samsung Q BrowseMetadata: {qmeta}");
    assert!(
        qmeta.contains("&lt;upnp:lastPlaybackPosition&gt;120000&lt;/upnp:lastPlaybackPosition&gt;")
            || qmeta.contains("<upnp:lastPlaybackPosition>120000</upnp:lastPlaybackPosition>"),
        "lastPlaybackPosition 120000: {qmeta}"
    );
    assert!(
        qmeta.contains("BM=120000"),
        "sec:dcmInfo BM= must be converted ms: {qmeta}"
    );
    let (st, kodi_bm) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner(&id.to_string(), "BrowseMetadata"),
    );
    assert_eq!(st, 200, "{kodi_bm}");
    assert!(
        kodi_bm.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
            || kodi_bm.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
        "Kodi lastPlaybackPosition 120: {kodi_bm}"
    );
    assert!(
        !kodi_bm.contains("BM=120000"),
        "Kodi must not see CONVERT_MS milliseconds: {kodi_bm}"
    );

    // 12. Search StartingIndex=-1 → 402
    let (st, search402) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Search",
        r#"<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>-1</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Search>"#,
    );
    assert_eq!(st, 500, "Search -1: {search402}");
    assert!(
        search402.contains("<errorCode>402</errorCode>"),
        "{search402}"
    );

    // 13. Samsung GET remaps Content-Type to video/x-mkv
    let (st, hdr, _) = raw_http(
        http,
        &format!("GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "samsung GET {hdr}");
    assert!(hdr_has(&hdr, "video/x-mkv"), "samsung mime: {hdr}");

    // 14. getcontentFeatures.dlna.org: 0 → 400
    let (st, hdr, _) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\ngetcontentFeatures.dlna.org: 0\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 400, "getcontentFeatures 0: {hdr}");

    // 15. transferMode Interactive on video (Kodi) → 406
    let (st, hdr, _) = raw_http(
        http,
        &format!("GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nUser-Agent: Kodi/21.0\r\ntransferMode.dlna.org: Interactive\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 406, "Interactive video: {hdr}");

    // 16. Accept-Language: en → Content-Language: en
    let (st, hdr, _) = raw_http(
        http,
        &format!("GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nAccept-Language: en\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "{hdr}");
    assert!(hdr_has(&hdr, "content-language: en"), "{hdr}");

    // 17. Icons: 200 + matching magic
    let (st, _hdr, png) = raw_http(
        http,
        &format!(
            "GET /icons/sm.png HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(st, 200);
    assert!(
        png.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
        "png magic: {:02x?}",
        &png[..png.len().min(8)]
    );
    let (st, _hdr, jpg) = raw_http(
        http,
        &format!(
            "GET /icons/sm.jpg HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(st, 200);
    assert!(
        jpg.len() >= 3 && jpg[0] == 0xff && jpg[1] == 0xd8,
        "jpeg magic: {:02x?}",
        &jpg[..jpg.len().min(8)]
    );

    // 18. Thumbnails for movie with poster — 200 jpeg
    let (st, hdr, thumb) = raw_http(
        http,
        &format!("GET /Thumbnails/{id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "thumbnail must be 200 for movie poster: {hdr}");
    assert!(hdr_has(&hdr, "image/jpeg"), "thumbnail content-type: {hdr}");
    assert!(
        thumb.len() >= 2 && thumb[0] == 0xff && thumb[1] == 0xd8,
        "thumbnail jpeg magic: {:02x?}",
        &thumb[..thumb.len().min(8)]
    );
    let (st, hdr, resized) = raw_http(
        http,
        &format!("GET /Resized/{id}.jpg?width=160,height=160 HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "resized GET: {hdr}");
    assert!(hdr_has(&hdr, "content-type: image/jpeg"), "{hdr}");
    assert!(hdr_has(&hdr, "transferMode.dlna.org: Interactive"), "{hdr}");
    assert!(
        hdr.contains("DLNA.ORG_PN=JPEG_TN") && hdr.contains("DLNA.ORG_CI=1"),
        "resized contentFeatures: {hdr}"
    );
    assert!(
        resized.len() >= 2 && resized[0] == 0xff && resized[1] == 0xd8,
        "resized jpeg magic"
    );

    // 19. Browse 1 / 1$4 / 3 — music and image trees exist
    let (st, music) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("1", "BrowseDirectChildren"),
    );
    assert_eq!(st, 200, "Browse 1: {music}");
    assert!(
        music.contains("All Music"),
        "Browse 1 missing All Music: {music}"
    );
    let (st, tracks) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("1$4", "BrowseDirectChildren"),
    );
    assert_eq!(st, 200, "Browse 1$4: {tracks}");
    assert!(
        tracks.contains("Fixture Track"),
        "Browse 1$4 missing Fixture Track: {tracks}"
    );
    let (st, pics) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        &browse_inner("3", "BrowseDirectChildren"),
    );
    assert_eq!(st, 200, "Browse 3: {pics}");
    assert!(
        pics.contains("All Pictures"),
        "Browse 3 missing All Pictures: {pics}"
    );

    // 20. Inbound NOTIFY MediaRenderer + Allegro-Software-RomPlug — server stays up
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let notify = format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST:239.255.255.250:1900\r\n\
         NT:urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
         NTS:ssdp:alive\r\n\
         LOCATION:http://127.0.0.1:{http}/rootDesc.xml\r\n\
         SERVER: Allegro-Software-RomPlug/1.0\r\n\
         \r\n"
    );
    sock.send_to(notify.as_bytes(), ("127.0.0.1", ssdp))
        .expect("send notify");
    let mut last_st = 0;
    let mut last_hdr = String::new();
    let mut ok = false;
    for _ in 0..10 {
        let (st, hdr, _) = raw_http(
            http,
            &format!("GET /status HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nConnection: close\r\n\r\n"),
        );
        last_st = st;
        last_hdr = hdr;
        if st == 200 {
            ok = true;
        } else {
            ok = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ok && last_st == 200,
        "status after renderer NOTIFY must stay 200 for ~500ms: {last_st} {last_hdr}"
    );
}

#[test]
fn remaining_dialect_e2e() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip remaining dialect e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    let http = http.saturating_add(5);
    let ssdp = ssdp.saturating_add(5);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let _srv = spawn_bin(http, ssdp);
    let items = wait_media_items(http, "Kodi/21.0", "2$8");
    assert!(items.contains("Fixture Movie"), "{items}");

    // Xbox Search: originals only (`@refID exists false`)
    let (st, xbox) = soap(
        http,
        "Xbox/360",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Search",
        r#"<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ContainerID>0</ContainerID><SearchCriteria>upnp:class derivedfrom "object.item.videoItem" and @refID exists false</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Search>"#,
    );
    assert_eq!(st, 200, "{xbox}");
    assert!(xbox.contains("Fixture Movie"), "Xbox exists false: {xbox}");
    assert!(!xbox.contains("refID="), "Xbox must drop aliases: {xbox}");

    // OR across audio/video
    let (st, or_xml) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Search",
        r#"<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ContainerID>0</ContainerID><SearchCriteria>(upnp:class derivedfrom "object.item.audioItem") or (upnp:class derivedfrom "object.item.videoItem")</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Search>"#,
    );
    assert_eq!(st, 200, "{or_xml}");
    assert!(or_xml.contains("Fixture Movie"), "{or_xml}");
    assert!(or_xml.contains("Fixture Track"), "{or_xml}");

    // Listed Filter without res@size omits size and <res>
    let (st, filt) = soap(
        http,
        "Kodi/21.0",
        "urn:schemas-upnp-org:service:ContentDirectory:1#Browse",
        r#"<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>dc:title,upnp:class</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#,
    );
    assert_eq!(st, 200, "{filt}");
    assert!(filt.contains("Fixture Movie"), "{filt}");
    assert!(
        !filt.contains(" size=&quot;") && !filt.contains(" size=\""),
        "{filt}"
    );
    assert!(
        !filt.contains("&lt;res ") && !filt.contains("<res "),
        "{filt}"
    );

    // SSDP ST leftover :10 must not reply
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(300))).ok();
    let bad_st = "M-SEARCH * HTTP/1.1\r\nHOST:127.0.0.1:11900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:service:ContentDirectory:10\r\n\r\n";
    sock.send_to(bad_st.as_bytes(), ("127.0.0.1", ssdp))
        .expect("send");
    let mut buf = [0u8; 2048];
    assert!(
        sock.recv_from(&mut buf).is_err(),
        "ContentDirectory:10 must not get an M-SEARCH reply"
    );
    let ok_st = "M-SEARCH * HTTP/1.1\r\nHOST:127.0.0.1:11900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:service:ContentDirectory:1\r\n\r\n";
    sock.send_to(ok_st.as_bytes(), ("127.0.0.1", ssdp))
        .expect("send");
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let (n, _) = sock.recv_from(&mut buf).expect("CD:1 reply");
    let reply = String::from_utf8_lossy(&buf[..n]);
    assert!(reply.contains("HTTP/1.1 200 OK"), "{reply}");
    assert!(reply.contains("ContentDirectory:1"), "{reply}");
}

/// Real Kodi/Platinum control-point walk (scripts/kodi_upnp_client.py).
/// UA, Filter, page size 200, NPT date rules, and resource pick come from
/// xbmc/xbmc lib/libUPnP — not a homemade User-Agent string.
#[test]
fn kodi_platinum_client_e2e() {
    let Some((http, ssdp)) = env_ports() else {
        eprintln!("skip kodi platinum e2e (RUSTY_DLNA_HTTP_PORT unset)");
        return;
    };
    let http = http.saturating_add(6);
    let ssdp = ssdp.saturating_add(6);
    assert!(!rusty_dlna_protocol::isolation::collides_with_live_ports(
        http, ssdp
    ));
    let _srv = spawn_bin(http, ssdp);
    let _ = wait_media_items(http, "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13", "2$8");
    let script = workspace().join("scripts/kodi_upnp_client.py");
    let out = Command::new("python3")
        .arg(&script)
        .arg("--http")
        .arg(http.to_string())
        .arg("--ssdp")
        .arg(ssdp.to_string())
        .output()
        .expect("python3 kodi_upnp_client.py");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Kodi/Platinum client failed status={:?}\n{stdout}{stderr}",
        out.status
    );
    assert!(stdout.contains("Kodi/Platinum walk OK"), "{stdout}{stderr}");
}
