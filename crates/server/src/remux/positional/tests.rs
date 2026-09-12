use super::*;
use crate::remux::tests::{growing_test_job, temp_dir, test_app};
use crate::remux::{stream_growing, RemuxState};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, server) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (client.unwrap(), server.unwrap().0)
}

#[test]
fn concurrent_positioned_ranges_leave_shared_cursor_unchanged_after_replacement() {
    let dir = temp_dir("positional-ranges");
    let bytes: Vec<u8> = (0..2 * READ_BYTES).map(|i| (i % 251) as u8).collect();
    let job = growing_test_job(&dir, 1, &bytes);
    let file = job.open_output().unwrap();
    let mut cursor = file.try_clone().unwrap();
    cursor.seek(SeekFrom::Start(137)).unwrap();
    std::fs::rename(&job.part, &job.dest).unwrap();
    std::fs::write(&job.part, b"replacement staging").unwrap();
    let replacement = dir.join("replacement");
    std::fs::write(&replacement, b"replacement published").unwrap();
    std::fs::rename(replacement, &job.dest).unwrap();
    std::thread::scope(|scope| {
        for reader in 0..8 {
            let file = &file;
            let bytes = &bytes;
            scope.spawn(move || {
                let mut buffer = vec![0; 32 * 1024];
                for turn in 0..128 {
                    let offset = (reader * 1777 + turn * 997) % (bytes.len() - buffer.len());
                    let count = read_chunk(file, &mut buffer, offset as u64, None).unwrap();
                    assert_eq!(&buffer[..count], &bytes[offset..offset + count]);
                }
            });
        }
    });
    assert_eq!(cursor.stream_position().unwrap(), 137);
    let mut byte = [0];
    cursor.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], bytes[137]);
    assert_eq!(read_chunk(&file, &mut byte, 4, Some(3)).unwrap(), 0);
    assert_eq!(
        read_chunk(&file, &mut byte, bytes.len() as u64, None).unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_http_ranges_and_segments_ignore_index_lock_and_cursor() {
    let dir = temp_dir("positional-http");
    let app = test_app(&dir, 1);
    let bytes: Arc<Vec<u8>> = Arc::new((0..4 * READ_BYTES).map(|i| (i % 251) as u8).collect());
    let job = growing_test_job(&dir, 2, &bytes);
    job.transition(RemuxState::Complete);
    let output = job.open_output().unwrap();
    let mut cursor = output.try_clone().unwrap();
    cursor.seek(SeekFrom::Start(17)).unwrap();
    let index_job = job.clone();
    let (locked, held) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _index = crate::lock_recover(&index_job.hls_index);
        locked.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    held.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut clients = tokio::task::JoinSet::new();
    for reader in 0..8 {
        let (mut client, mut server) = socket_pair().await;
        let app = app.clone();
        let job = job.clone();
        let bytes = bytes.clone();
        clients.spawn(async move {
            let start = reader * 997;
            let length = 2 * READ_BYTES + reader * 71;
            let send = async {
                if reader % 2 == 0 {
                    stream_growing(&app, &mut server, &job, start as u64, Some((start + length - 1) as u64)).await.unwrap();
                } else {
                    let req = rusty_dlna_http::HttpRequest::parse_headers(&format!(
                        "GET /web/media/2.m4s?delivery=mse_segment&hls_offset={start}&hls_length={length} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                    )).unwrap();
                    crate::remux::serve_hls_resource(&app, &mut server, &req, &job, "video/iso.segment", false).await.unwrap();
                }
                drop(server);
            };
            let receive = async {
                let mut received = Vec::new();
                client.read_to_end(&mut received).await.unwrap();
                let body = if reader % 2 == 0 { &received[..] } else {
                    let offset = received.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
                    &received[offset..]
                };
                assert_eq!(body, &bytes[start..start + length]);
            };
            tokio::join!(send, receive);
        });
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 17);
}

#[tokio::test]
async fn growing_eof_resumes_then_published_eof_finishes() {
    let dir = temp_dir("positional-growth");
    let app = test_app(&dir, 1);
    let job = growing_test_job(&dir, 3, b"first");
    let (mut client, mut server) = socket_pair().await;
    let send_job = job.clone();
    let send = tokio::spawn(async move {
        stream_growing(&app, &mut server, &send_job, 0, None)
            .await
            .unwrap();
    });
    let mut first = [0; 5];
    client.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"first");
    assert!(!send.is_finished());
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&job.part)
        .unwrap();
    writer.write_all(b"second").unwrap();
    std::fs::rename(&job.part, &job.dest).unwrap();
    job.transition(RemuxState::Complete);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    send.await.unwrap();
    assert_eq!(rest, b"second");
}

#[tokio::test]
async fn truncation_and_terminal_producer_states_end_waiting_delivery() {
    for terminal in ["truncate", "fail", "cancel"] {
        let dir = temp_dir(terminal);
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 4, b"first");
        let (mut client, mut server) = socket_pair().await;
        let send_job = job.clone();
        let send = tokio::spawn(async move {
            stream_growing(&app, &mut server, &send_job, 0, None)
                .await
                .map_err(|error| error.to_string())
        });
        let mut first = [0; 5];
        client.read_exact(&mut first).await.unwrap();
        match terminal {
            "truncate" => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&job.part)
                    .unwrap()
                    .set_len(2)
                    .unwrap();
                job.changed.notify_waiters();
            }
            "fail" => job.transition(RemuxState::Failed("producer stopped".into())),
            _ => job.cancel(),
        }
        let result = tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .unwrap()
            .unwrap();
        if terminal == "truncate" {
            assert!(result.unwrap_err().contains("promised range"));
        } else {
            result.unwrap();
        }
    }
}

#[tokio::test]
async fn slow_socket_cancellation_releases_delivery_without_waiting_write_deadline() {
    let dir = temp_dir("positional-slow");
    let app = test_app(&dir, 1);
    let job = growing_test_job(&dir, 5, &[]);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&job.part)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let (mut client, mut server) = socket_pair().await;
    let send_job = job.clone();
    let send = tokio::spawn(async move {
        stream_growing(&app, &mut server, &send_job, 0, None)
            .await
            .unwrap();
    });
    client.read_u8().await.unwrap();
    // The socket cannot drain this file without a reader. The producer owns no
    // per-client helper and the socket retains only its one fixed read buffer.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!send.is_finished());
    let started = Instant::now();
    job.cancel();
    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .unwrap()
        .unwrap();
    eprintln!(
        "slow_socket_cancellation_us={}",
        started.elapsed().as_micros()
    );
    assert_eq!(Arc::strong_count(&job), 1);
}

#[tokio::test]
async fn slow_socket_keeps_its_write_deadline() {
    let dir = temp_dir("positional-write-deadline");
    let mut app = test_app(&dir, 1);
    Arc::get_mut(&mut app).unwrap().cfg.write_timeout_secs = 1;
    let job = growing_test_job(&dir, 6, &[]);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&job.part)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let (mut client, mut server) = socket_pair().await;
    let send_job = job.clone();
    let send = tokio::spawn(async move {
        stream_growing(&app, &mut server, &send_job, 0, None)
            .await
            .unwrap();
    });
    client.read_u8().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), send)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(Arc::strong_count(&job), 1);
}

#[tokio::test]
async fn complete_output_cannot_silently_shorten_promised_range() {
    let dir = temp_dir("positional-promised-range");
    let app = test_app(&dir, 1);
    let job = growing_test_job(&dir, 7, b"short");
    job.transition(RemuxState::Complete);
    let (_client, mut server) = socket_pair().await;
    let error = stream_growing(&app, &mut server, &job, 0, Some(99))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("promised range"));
}

fn usage() -> libc::rusage {
    let mut usage = std::mem::MaybeUninit::uninit();
    // SAFETY: getrusage receives writable storage for exactly one rusage and
    // successful return initializes it. Failure is asserted before reading it.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(result, 0);
    // SAFETY: the successful getrusage call above initialized this value.
    unsafe { usage.assume_init() }
}

fn cpu_seconds(usage: &libc::rusage) -> f64 {
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

/// Matched delivery microbenchmark, intentionally synthetic (no media decode).
/// Run alone with --ignored --nocapture --test-threads=1, preferably release.
/// Each rotated sample sends 8 GiB over four loopback sockets while another
/// blocking worker reads the same fixed sparse metadata workload. Legacy is
/// the pre-P05 mutex/stat/seek/read loop, including its 64 KiB buffer. All
/// positioned variants keep 64 KiB first reads; only sustained size varies.
#[test]
#[ignore = "8 GiB x 20 matched streaming/indexing microbenchmark"]
fn streaming_delivery_benchmark() {
    const FILE_BYTES: usize = 64 * 1024 * 1024;
    const PER_READER: u64 = 2 * 1024 * 1024 * 1024;
    const READERS: u64 = 4;
    const INDEX_READS: usize = 131_072;
    let dir = temp_dir("delivery-benchmark");
    let path = dir.join("warm-synthetic-output");
    let mut writer = File::create(&path).unwrap();
    let block = vec![0x67; 1024 * 1024];
    for _ in 0..FILE_BYTES / block.len() {
        writer.write_all(&block).unwrap();
    }
    writer.sync_all().unwrap();
    drop(writer);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap();
    for sample in 0..5 {
        for order in 0..4 {
            let variant = (sample + order) % 4;
            let chunk = [64, 64, 256, 1024][variant] * 1024;
            let legacy = variant == 0;
            let file = Arc::new(File::open(&path).unwrap());
            let cursor = Arc::new(Mutex::new(file.try_clone().unwrap()));
            let before = usage();
            let started = Instant::now();
            let first = runtime.block_on(async {
                let index_file = file.clone();
                let index_cursor = cursor.clone();
                let index = tokio::task::spawn_blocking(move || {
                    let mut bytes = [0; 4096];
                    // Identical 512 MiB metadata traffic in all variants. The
                    // index lock is held for one read, without injected sleeps.
                    for item in 0..INDEX_READS {
                        let offset = ((item * 65521) % (FILE_BYTES - bytes.len())) as u64;
                        if legacy {
                            let mut file = crate::lock_recover(&index_cursor);
                            file.seek(SeekFrom::Start(offset)).unwrap();
                            file.read_exact(&mut bytes).unwrap();
                        } else {
                            index_file.read_exact_at(&mut bytes, offset).unwrap();
                        }
                        std::hint::black_box(bytes[0]);
                    }
                });
                let mut streams = tokio::task::JoinSet::new();
                for _ in 0..READERS {
                    let (mut receiver, mut sender) = socket_pair().await;
                    let file = file.clone();
                    let cursor = cursor.clone();
                    streams.spawn(async move {
                        let receive = async {
                            let mut bytes = vec![0; 256 * 1024];
                            let mut got = 0;
                            while got < PER_READER {
                                let count = receiver.read(&mut bytes).await.unwrap();
                                assert!(count > 0);
                                got += count as u64;
                            }
                            assert_eq!(got, PER_READER);
                        };
                        let send = async {
                            use tokio::io::AsyncWriteExt;
                            let mut bytes = vec![0; chunk];
                            let mut sent = 0;
                            let mut first = None;
                            while sent < PER_READER {
                                let offset = sent % FILE_BYTES as u64;
                                let read_file = file.clone();
                                let read_cursor = cursor.clone();
                                let (returned, count) = tokio::task::spawn_blocking(move || {
                                    let count = if legacy {
                                        let mut file = crate::lock_recover(&read_cursor);
                                        let available =
                                            file.metadata().unwrap().len().saturating_sub(offset);
                                        let want = available.min(bytes.len() as u64) as usize;
                                        file.seek(SeekFrom::Start(offset)).unwrap();
                                        file.read(&mut bytes[..want]).unwrap()
                                    } else {
                                        let limit = if sent == 0 { 64 * 1024 } else { bytes.len() };
                                        read_chunk(
                                            &read_file,
                                            &mut bytes[..limit],
                                            offset,
                                            Some(FILE_BYTES as u64 - 1),
                                        )
                                        .unwrap()
                                    };
                                    (bytes, count)
                                })
                                .await
                                .unwrap();
                                bytes = returned;
                                assert!(count > 0);
                                sender.write_all(&bytes[..count]).await.unwrap();
                                first.get_or_insert_with(|| started.elapsed());
                                sent += count as u64;
                            }
                            first.unwrap()
                        };
                        let (first, ()) = tokio::join!(send, receive);
                        first
                    });
                }
                let mut first = Vec::new();
                while let Some(result) = streams.join_next().await {
                    first.push(result.unwrap());
                }
                index.await.unwrap();
                first
            });
            let elapsed = started.elapsed().as_secs_f64();
            let after = usage();
            let gib = (PER_READER * READERS) as f64 / 1_073_741_824.0;
            let first_us: Vec<_> = first.iter().map(|time| time.as_micros()).collect();
            eprintln!(
                "{}",
                serde_json::json!({
                    "sample": sample, "legacy": legacy, "read_bytes": chunk,
                    "readers": READERS, "gib": gib, "seconds": elapsed,
                    "gib_per_second": gib / elapsed,
                    "cpu_seconds_per_gib": (cpu_seconds(&after) - cpu_seconds(&before)) / gib,
                    "voluntary_switches_per_gib": (after.ru_nvcsw - before.ru_nvcsw) as f64 / gib,
                    "involuntary_switches_per_gib": (after.ru_nivcsw - before.ru_nivcsw) as f64 / gib,
                    "first_write_us": first_us, "peak_rss_kib_process": after.ru_maxrss,
                    "index_bytes": INDEX_READS * 4096,
                })
            );
        }
    }
}
