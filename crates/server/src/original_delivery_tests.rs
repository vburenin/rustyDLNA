//! Real-socket oracles for original-file delivery, separate from growing remux.
use super::*;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use tokio::io::AsyncReadExt;

const CAP: usize = 8 * 1024 * 1024;

fn configured(source: &Path) -> App {
    let mut app = testdata_app();
    app.remaps.clear();
    app.scan_cfg.media_roots.clear();
    app.scan_cfg.media_dirs = vec![source.parent().unwrap().to_owned()];
    let mut item = movie_fixture(&app);
    item.path = source.to_owned();
    item.detail_id = 9_100_010;
    item.object_id = "original-delivery".into();
    item.size = source.metadata().unwrap().len();
    let mut catalog = write_recover(&app.catalog);
    catalog
        .by_detail
        .insert(item.detail_id, item.object_id.clone());
    catalog.items.insert(item.object_id.clone(), item);
    drop(catalog);
    app
}

fn request(method: &str, range: Option<&str>) -> String {
    let range = range
        .map(|value| format!("Range: {value}\r\n"))
        .unwrap_or_default();
    format!("{method} /MediaItems/9100010.mkv HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n{range}\r\n")
}

fn split(wire: &[u8]) -> (String, &[u8]) {
    let end = wire
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    (
        String::from_utf8(wire[..end].to_vec()).unwrap(),
        &wire[end..],
    )
}

async fn pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, server) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (client.unwrap(), server.unwrap().0)
}

#[tokio::test]
async fn original_socket_bytes_ranges_head_and_cutoff() {
    let tree = TestTree::new("original-socket");
    let path = tree.path().join("original.mkv");
    // Non-periodic across read boundaries: shifted reads cannot accidentally match.
    let bytes: Vec<u8> = (0..CAP + 65539)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    std::fs::write(&path, &bytes).unwrap();
    let app = Arc::new(configured(&path));
    for (range, start, length) in [
        (None, 0, bytes.len()),
        (Some("bytes=17-119".to_owned()), 17, 103),
        (Some("bytes=-65539".to_owned()), CAP, 65539),
        (
            Some(format!("bytes=11-{}", bytes.len() - 1)),
            11,
            bytes.len() - 11,
        ),
        (Some(format!("bytes=0-{}", CAP - 2)), 0, CAP - 1),
        (Some(format!("bytes=0-{}", CAP - 1)), 0, CAP),
        (Some(format!("bytes=0-{CAP}")), 0, CAP + 1),
        (
            Some(format!("bytes=-{}", CAP + 1)),
            bytes.len() - CAP - 1,
            CAP + 1,
        ),
    ] {
        for method in ["GET", "HEAD"] {
            let wire = raw_connection(
                app.clone(),
                request(method, range.as_deref()).as_bytes(),
                false,
            )
            .await;
            let (headers, body) = split(&wire);
            let status = if range.is_some() { 206 } else { 200 };
            assert!(
                headers.starts_with(&format!("HTTP/1.1 {status} ")),
                "{headers}"
            );
            assert!(headers
                .to_ascii_lowercase()
                .contains("connection: close\r\n"));
            assert!(
                headers.contains(&format!("Content-Length: {length}\r\n")),
                "{headers}"
            );
            if range.is_some() {
                assert!(headers.contains(&format!(
                    "Content-Range: bytes {start}-{}/{}\r\n",
                    start + length - 1,
                    bytes.len()
                )));
            }
            assert_eq!(
                body,
                if method == "HEAD" {
                    &[]
                } else {
                    &bytes[start..start + length]
                }
            );
        }
    }
    for (range, status) in [
        ("bytes=abc", 400),
        ("bytes=3-2", 400),
        ("bytes=0-1,4-5", 400),
        ("bytes=999999999-", 416),
    ] {
        let wire = raw_connection(app.clone(), request("GET", Some(range)).as_bytes(), false).await;
        let (headers, _) = split(&wire);
        assert!(
            headers.starts_with(&format!("HTTP/1.1 {status} ")),
            "{range}: {headers}"
        );
    }
    let bytes = Arc::new(bytes);
    let mut readers = tokio::task::JoinSet::new();
    for reader in 0..4 {
        let app = app.clone();
        let bytes = bytes.clone();
        readers.spawn(async move {
            let start = reader * 17;
            let range = format!("bytes={start}-");
            let wire = raw_connection(app, request("GET", Some(&range)).as_bytes(), false).await;
            let (headers, body) = split(&wire);
            assert!(headers.starts_with("HTTP/1.1 206 "));
            assert_eq!(body.len(), bytes.len() - start);
            assert!(
                body == &bytes[start..],
                "concurrent HTTP reader {reader} received another offset"
            );
        });
    }
    while let Some(reader) = readers.join_next().await {
        reader.unwrap();
    }
}

#[tokio::test]
async fn original_socket_empty_get_and_head_have_zero_length() {
    let tree = TestTree::new("original-empty");
    let path = tree.path().join("empty.mkv");
    std::fs::write(&path, []).unwrap();
    let app = Arc::new(configured(&path));
    for method in ["GET", "HEAD"] {
        let wire = raw_connection(app.clone(), request(method, None).as_bytes(), false).await;
        let (headers, body) = split(&wire);
        assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
        assert!(headers.contains("Content-Length: 0\r\n"), "{headers}");
        assert!(body.is_empty());
    }
    let wire = raw_connection(app, request("GET", Some("bytes=0-")).as_bytes(), false).await;
    assert!(split(&wire).0.starts_with("HTTP/1.1 416 "));
}

#[tokio::test]
async fn original_stream_truncation_is_an_error_with_only_source_bytes() {
    let tree = TestTree::new("original-truncated");
    let path = tree.path().join("original.mkv");
    std::fs::write(&path, b"short").unwrap();
    let app = configured(&path);
    let (mut client, mut server) = pair().await;
    let send = async {
        let result = crate::lifecycle::stream_open_file_range(
            &app,
            &mut server,
            File::open(&path).unwrap(),
            0,
            99,
        )
        .await;
        drop(server);
        result
    };
    let receive = async {
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"short");
    };
    let (result, ()) = tokio::join!(send, receive);
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn original_streams_pin_replaced_inode_and_do_not_share_offsets() {
    let tree = TestTree::new("original-offsets");
    let path = tree.path().join("original.mkv");
    let bytes: Arc<Vec<u8>> = Arc::new(
        (0..2 * 1024 * 1024)
            .map(|i| ((i * 31 + i / 251) % 256) as u8)
            .collect(),
    );
    std::fs::write(&path, bytes.as_slice()).unwrap();
    let app = Arc::new(configured(&path));
    let opened = rusty_dlna_scan::open_allowed_file(&path, &app.scan_cfg).unwrap();
    let mut cursor = opened.file.try_clone().unwrap();
    cursor.seek(SeekFrom::Start(137)).unwrap();
    let replacement = tree.path().join("replacement");
    std::fs::write(&replacement, b"replacement must not be served").unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for reader in 0..8 {
        let (mut client, mut server) = pair().await;
        let app = app.clone();
        let file = opened.file.try_clone().unwrap();
        let bytes = bytes.clone();
        tasks.spawn(async move {
            let start = reader * 997;
            let length = 1024 * 1024 + reader * 71;
            let send = async {
                let result = crate::lifecycle::stream_open_file_range(
                    &app,
                    &mut server,
                    file,
                    start as u64,
                    (start + length - 1) as u64,
                )
                .await;
                drop(server);
                result.unwrap();
            };
            let receive = async {
                let mut received = Vec::new();
                client.read_to_end(&mut received).await.unwrap();
                assert_eq!(received.len(), length, "reader {reader}");
                assert!(
                    received == bytes[start..start + length],
                    "reader {reader} received another offset or inode"
                );
            };
            tokio::join!(send, receive);
        });
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 137);
}

#[tokio::test]
async fn original_partial_reads_and_eagain_preserve_bytes_for_progressing_reader() {
    let tree = TestTree::new("original-partial");
    let path = tree.path().join("original.mkv");
    let expected: Vec<u8> = (0..512 * 1024)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    std::fs::write(&path, &expected).unwrap();
    let app = configured(&path);
    let (mut client, mut server) = pair().await;
    socket2::SockRef::from(&server)
        .set_send_buffer_size(4096)
        .unwrap();
    let filler = [0x9a; 65536];
    let mut queued = 0;
    server.writable().await.unwrap();
    loop {
        match server.try_write(&filler) {
            Ok(count) => {
                queued += count;
                assert!(queued < 4 * 1024 * 1024);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            result => panic!("fill socket: {result:?}"),
        }
    }
    assert!(
        queued > 0,
        "forced partial-write/EAGAIN control did not fill the socket"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let send = async {
        crate::file_delivery::stream_with_read(
            &app,
            &mut server,
            File::open(&path).unwrap(),
            0,
            (expected.len() - 1) as u64,
            move |file, buffer, offset| {
                if observed.fetch_add(1, Ordering::Relaxed).is_multiple_of(5) {
                    return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
                }
                // Force short successful reads at the actual file boundary.
                let count = buffer.len().min(997);
                file.read_at(&mut buffer[..count], offset)
            },
        )
        .await
        .unwrap();
        drop(server);
    };
    let receive = async {
        let mut actual = Vec::new();
        let mut chunk = [0; 4093];
        loop {
            let count = client.read(&mut chunk).await.unwrap();
            if count == 0 {
                break;
            }
            actual.extend_from_slice(&chunk[..count]);
            // A bounded progressing reader deliberately crosses partial writes.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(&actual[..queued], vec![0x9a; queued]);
        assert_eq!(&actual[queued..], &expected);
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(send, receive);
    })
    .await
    .unwrap();
    assert!(calls.load(Ordering::Relaxed) > expected.len() / 997);
    assert_eq!(
        app.original_reads.available_permits(),
        app.cfg.max_connections
    );
}

#[tokio::test]
async fn original_socket_writer_retries_partial_writes_after_forced_eagain() {
    use std::future::Future;
    use std::task::Poll;
    let app = testdata_app();
    let (mut client, mut server) = pair().await;
    socket2::SockRef::from(&server)
        .set_send_buffer_size(4096)
        .unwrap();
    server.writable().await.unwrap();
    let filler = [0xa7; 65536];
    let mut queued = 0;
    loop {
        match server.try_write(&filler) {
            Ok(count) => {
                queued += count;
                assert!(queued < 4 * 1024 * 1024);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            result => panic!("fill socket: {result:?}"),
        }
    }
    assert!(queued > 0);
    let payload: Vec<u8> = (0..512 * 1024)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    let mut write = Box::pin(crate::socket_write_all(&app, &mut server, &payload));
    // Poll the production write while its actual kernel socket is full and the
    // receiver is paused. A WouldBlock return or ignored tail cannot pass.
    std::future::poll_fn(|context| {
        assert!(write.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    let receive = async {
        let mut received = vec![0; queued + payload.len()];
        for chunk in received.chunks_mut(4093) {
            client.read_exact(chunk).await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(&received[..queued], vec![0xa7; queued]);
        assert_eq!(&received[queued..], payload);
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let (result, ()) = tokio::join!(write.as_mut(), receive);
        result.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn original_promised_http_range_reports_truncation_after_headers() {
    let tree = TestTree::new("original-http-truncate");
    let path = tree.path().join("original.mkv");
    let mut bytes = vec![0x31; CAP + 19];
    bytes[..19].copy_from_slice(b"independent prefix!");
    std::fs::write(&path, &bytes).unwrap();
    let app = configured(&path);
    let response = app.handle(&req(&request("GET", None)));
    let range = response.file_range.as_ref().expect("streaming response");
    let (mut client, mut server) = pair().await;
    crate::socket_write_http_response(&app, &mut server, &response)
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(19)
        .unwrap();
    let send = async {
        let result = crate::lifecycle::stream_open_file_range(
            &app,
            &mut server,
            range.file.try_clone().unwrap(),
            range.start,
            range.end,
        )
        .await;
        drop(server);
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    };
    let receive = async {
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await.unwrap();
        let (headers, body) = split(&wire);
        assert!(headers.contains(&format!("Content-Length: {}\r\n", bytes.len())));
        assert!(headers
            .to_ascii_lowercase()
            .contains("connection: close\r\n"));
        assert_eq!(body, &bytes[..19]);
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(send, receive);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn original_stopped_reader_deadline_disconnect_and_shutdown_release_resources() {
    let tree = TestTree::new("original-stopped");
    let path = tree.path().join("original.mkv");
    let bytes: Vec<u8> = (0..262181)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    std::fs::write(&path, &bytes).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    for action in ["deadline", "disconnect", "shutdown"] {
        let mut configured = configured(&path);
        configured.cfg.write_timeout_secs = 1;
        let app = Arc::new(configured);
        let (mut client, mut server) = pair().await;
        socket2::SockRef::from(&server)
            .set_send_buffer_size(4096)
            .unwrap();
        let file = File::open(&path).unwrap();
        let owner = app.clone();
        let send = tokio::spawn(async move {
            crate::lifecycle::stream_open_file_range(
                &owner,
                &mut server,
                file,
                0,
                64 * 1024 * 1024 - 1,
            )
            .await
        });
        client.read_u8().await.unwrap();
        // A separate real HTTP request progresses while this reader is stopped.
        let response = raw_connection(
            app.clone(),
            b"GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            false,
        )
        .await;
        assert!(response.starts_with(b"HTTP/1.1 200 "));
        assert!(!send.is_finished());
        if action == "deadline" {
            let (mut progressing, mut writer) = pair().await;
            socket2::SockRef::from(&writer)
                .set_send_buffer_size(4096)
                .unwrap();
            let transfer = crate::lifecycle::stream_open_file_range(
                &app,
                &mut writer,
                File::open(&path).unwrap(),
                37,
                bytes.len() as u64 - 1,
            );
            let receive = async {
                let mut got = vec![0; bytes.len() - 37];
                for chunk in got.chunks_mut(8192) {
                    progressing.read_exact(chunk).await.unwrap();
                    // Explicit client-rate throttling, independent of readiness.
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                assert_eq!(got, bytes[37..]);
            };
            tokio::time::timeout(Duration::from_secs(1), async {
                let (sent, ()) = tokio::join!(transfer, receive);
                sent.unwrap();
            })
            .await
            .unwrap();
            assert!(
                !send.is_finished(),
                "stopped reader did not overlap the progressing reader"
            );
        }
        let start = Instant::now();
        let client = match action {
            "disconnect" => {
                drop(client);
                None
            }
            "shutdown" => {
                app.scan_control.cancellation.cancel();
                Some(client)
            }
            _ => {
                let result = tokio::time::timeout(Duration::from_secs(3), send)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
                drop(client);
                assert_eq!(
                    app.original_reads.available_permits(),
                    app.cfg.max_connections
                );
                continue;
            }
        };
        assert!(tokio::time::timeout(Duration::from_secs(1), send)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        eprintln!(
            "original_{action}_cleanup_us={}",
            start.elapsed().as_micros()
        );
        drop(client);
        assert_eq!(
            app.original_reads.available_permits(),
            app.cfg.max_connections
        );
        assert_eq!(Arc::strong_count(&app), 1);
    }
}

struct ReadLatch(Arc<(Mutex<bool>, std::sync::Condvar)>);
impl Drop for ReadLatch {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
}

#[tokio::test]
async fn original_blocked_disk_leaves_listener_responsive_and_retains_admission_until_reaped() {
    let tree = TestTree::new("original-slow-disk");
    let path = tree.path().join("original.mkv");
    std::fs::write(&path, b"pinned disk bytes").unwrap();
    for stop in ["abort", "timeout", "shutdown"] {
        let mut configured = configured(&path);
        configured.cfg.write_timeout_secs = 1;
        configured.original_reads = Arc::new(tokio::sync::Semaphore::new(1));
        let app = Arc::new(configured);
        let release = ReadLatch(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        let held = release.0.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started)));
        let (mut client, mut server) = pair().await;
        let owner = app.clone();
        let file = File::open(&path).unwrap();
        let send = tokio::spawn(async move {
            crate::file_delivery::stream_with_read(
                &owner,
                &mut server,
                file,
                0,
                16,
                move |file, buffer, offset| {
                    started.lock().unwrap().take().unwrap().send(()).unwrap();
                    let (guard, timeout) = held
                        .1
                        .wait_timeout_while(
                            held.0.lock().unwrap(),
                            Duration::from_secs(10),
                            |released| !*released,
                        )
                        .unwrap();
                    assert!(
                        !timeout.timed_out(),
                        "async listener could not release the disk read"
                    );
                    drop(guard);
                    file.read_at(buffer, offset)
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        assert!(!send.is_finished());
        assert_eq!(app.original_reads.available_permits(), 0);
        let wire = raw_connection(
            app.clone(),
            b"GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            false,
        )
        .await;
        assert!(wire.starts_with(b"HTTP/1.1 200 "));
        match stop {
            "abort" => send.abort(),
            "shutdown" => app.scan_control.cancellation.cancel(),
            _ => {}
        }
        let result = tokio::time::timeout(Duration::from_secs(3), send)
            .await
            .unwrap();
        match stop {
            "abort" => assert!(result.unwrap_err().is_cancelled()),
            "timeout" => assert_eq!(
                result.unwrap().unwrap_err().kind(),
                std::io::ErrorKind::TimedOut
            ),
            _ => assert_eq!(
                result.unwrap().unwrap_err().kind(),
                std::io::ErrorKind::Interrupted
            ),
        }
        // Kernel reads cannot be interrupted by dropping their async observer.
        // The worker retains its sole permit, descriptor and bounded buffer.
        assert_eq!(app.original_reads.available_permits(), 0);
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        assert!(
            bytes.is_empty(),
            "a cancelled observer emitted late disk bytes"
        );
        drop(release);
        let permit = tokio::time::timeout(Duration::from_secs(2), app.original_reads.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(app.original_reads.available_permits(), 1);
        assert_eq!(Arc::strong_count(&app), 1);
    }
}

/// The Python benchmark supplies an owned file and runs this release test in a
/// separate process. Only listener/request/delivery production code is measured;
/// catalog startup and the client CPU are outside its timed interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in release socket benchmark driven by scripts/direct-delivery-benchmark.py"]
async fn original_delivery_benchmark_server() {
    use std::io::Write;
    let path =
        PathBuf::from(std::env::var_os("RUSTY_DLNA_BENCH_SOURCE").expect("benchmark source"));
    let app = Arc::new(configured(&path));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    println!(
        "DIRECT_BENCH_READY {}",
        listener.local_addr().unwrap().port()
    );
    std::io::stdout().flush().unwrap();
    let mut stop =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.recv() => break,
            _ = tokio::time::sleep(Duration::from_secs(120)) => panic!("benchmark idle timeout"),
            Some(result) = tasks.join_next(), if !tasks.is_empty() => { result.unwrap(); },
            connection = listener.accept() => {
                let (socket, peer) = connection.unwrap();
                let app = app.clone();
                tasks.spawn(async move { let _ = handle_conn(app, socket, peer).await; });
            }
        }
    }
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        assert!(result.is_ok() || result.unwrap_err().is_cancelled());
    }
}
