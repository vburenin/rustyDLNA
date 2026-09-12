use super::*;
use rusty_dlna_scan::{WebMediaKind, WebMediaSort};

fn controlled_pool(readers: usize) -> (TestTree, Arc<DbPool>) {
    let tree = TestTree::new("query-budget");
    let pool = Arc::new(DbPool::open(&tree.path().join("files.db"), readers).unwrap());
    (tree, pool)
}

#[test]
fn reader_admission_cancels_with_all_readers_occupied_and_returns_every_lease() {
    let (_tree, pool) = controlled_pool(4);
    let entered = Arc::new(std::sync::Barrier::new(5));
    let release = Arc::new(std::sync::Barrier::new(5));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let pool = pool.clone();
            let entered = entered.clone();
            let release = release.clone();
            scope.spawn(move || {
                pool.read(|_| {
                    entered.wait();
                    release.wait();
                    Ok(())
                })
                .unwrap()
            });
        }
        entered.wait();
        let control = QueryControl::new(Default::default());
        let cancellation = control.cancellation.clone();
        let query_pool = pool.clone();
        let waiter = scope.spawn(move || {
            let _scope = enter_query_scope(control);
            query_pool.read::<()>(|_| panic!("cancelled waiter must never acquire a reader"))
        });
        let until = Instant::now() + Duration::from_secs(1);
        while pool.metrics().read_waiters == 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let start = Instant::now();
        cancellation.cancel();
        assert!(waiter.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_millis(250));
        assert_eq!(pool.metrics().read_waiters, 0);
        release.wait();
    });
    assert_eq!(pool.metrics().readers_available, 4);
    assert!(pool.read(|db| db.get_update_id()).is_ok());
}

#[test]
fn sqlite_execution_deadline_and_late_cancellation_leave_reader_reusable() {
    let (_tree, pool) = controlled_pool(1);
    let mut control = QueryControl::new(Default::default());
    control.deadline = Instant::now() + Duration::from_millis(40);
    let cancellation = control.cancellation.clone();
    let start = Instant::now();
    {
        let _scope = enter_query_scope(control);
        let result = pool.read(|db| {
            let tx = db.transaction()?;
            tx.query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))
        });
        assert!(
            matches!(result, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted)
        );
    }
    assert!(start.elapsed() < Duration::from_millis(300));
    assert_eq!(pool.metrics().readers_available, 1);
    // Cancel the old owner only after the next lease has acquired this reader.
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let fresh = scope.spawn(move || pool.read(|db| {
            entered_tx.send(()).unwrap();
            proceed_rx.recv().unwrap();
            let tx = db.transaction()?;
            tx.query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))
        }));
        entered_rx.recv().unwrap();
        cancellation.cancel();
        proceed_tx.send(()).unwrap();
        assert_eq!(fresh.join().unwrap().unwrap(), 50_005_000);
    });
}

#[test]
fn reader_wait_deadline_expires_without_starting_a_second_query() {
    let (_tree, pool) = controlled_pool(1);
    let mut control = QueryControl::new(Default::default());
    control.deadline = Instant::now() + Duration::from_millis(40);
    pool.read(|_| {
        let _scope = enter_query_scope(control);
        assert!(pool
            .read::<()>(|_| panic!("deadline must expire in admission"))
            .is_err());
        Ok(())
    })
    .unwrap();
    assert_eq!(pool.metrics().readers_available, 1);
    assert_eq!(pool.metrics().read_waiters, 0);
}

#[test]
fn controlled_sort_abandons_private_work_on_cancellation() {
    let control = QueryControl::new(Default::default());
    let mut values: Vec<_> = (0..50_000).rev().collect();
    let original = values.clone();
    let comparisons = std::cell::Cell::new(0usize);
    assert_eq!(
        controlled_sort_by(&mut values, &control, |a, b| {
            comparisons.set(comparisons.get() + 1);
            if comparisons.get() == 1_000 {
                control.cancellation.cancel();
            }
            a.cmp(b)
        }),
        Err(QueryStopped::Cancelled)
    );
    assert_eq!(values, original);
    assert!(comparisons.get() < 1_300);
}

#[test]
fn browser_api_database_and_fallback_share_unicode_domain_order_and_paging() {
    use std::os::unix::ffi::OsStringExt;
    let mut app = testdata_app();
    let db_path = app.scan_cfg.db_path.clone().unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let rows = [
        (800001, "ÉTÉ", "ФИЛЬМ", "éTÉ", "plain.mp4"),
        (800002, "été", "Артист", "Été", "literal%_\\.mp4"),
        (800003, "ФИЛЬМ", "ÉTÉ", "альбом", "COMBINING-e\u{301}.mp4"),
        (800004, "E\u{301}te", "Фильм", "Альбом", "é-file.mp4"),
        (800005, "ALIAS different", "ФИЛЬМ", "album", "alias.mp4"),
        (800006, "", "", "", "raw.mp4"),
    ];
    for (index, (id, title, artist, album, filename)) in rows.iter().enumerate() {
        let path = if *id == 800006 {
            PathBuf::from(std::ffi::OsString::from_vec(
                b"/PARENT_ONLY/RaW\xffFILE.mp4".to_vec(),
            ))
        } else {
            Path::new("/PARENT_ONLY").join(filename)
        };
        conn.execute("INSERT INTO DETAILS (ID, PATH, TITLE, ARTIST, ALBUM_ARTIST, ALBUM, MIME, DATE, DISC, TRACK, DEVICE, INODE) VALUES (?1, ?2, ?3, ?4, 'A\u{301}RTIST', ?5, 'video/mp4', '2026-01-01', 1, ?6, 99, ?7)",
            rusqlite::params![id, rusty_dlna_scan::path_to_db(&path), title, artist, album, (index % 2) as i64, if *id == 800005 { 800001 } else { *id }]).unwrap();
        // The reference is inserted first and has a smaller ID; the canonical
        // object must still win, including its empty-detail-title fallback.
        conn.execute("INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID, NAME, REF_ID) VALUES (?1, '64', 'item.videoItem', ?2, 'Wrong reference title', ?3)",
            rusqlite::params![format!("a-ref-{id}"), id, format!("z-canonical-{id}")]).unwrap();
        conn.execute("INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID, NAME) VALUES (?1, ?3, 'item.videoItem', ?2, ?4)",
            rusqlite::params![format!("z-canonical-{id}"), id,
                if *id == 800004 { rusty_dlna_protocol::object_id::VIDEO_SERIES_ID } else { "64" },
                if *id == 800004 { "SERIES ONLY" } else { "CANONICAL fallback" }]).unwrap();
    }
    drop(conn);
    let catalog = LibraryDb::open(&db_path).unwrap().load_catalog().unwrap();
    *write_recover(&app.catalog) = catalog;
    let pool = app.db_pool.take();
    let mut nonempty = 0;
    for query in [
        "",
        "été",
        "ФиЛьМ",
        "E\u{301}",
        "é",
        "a\u{301}rtist",
        "%",
        "_",
        "\\",
        "PARENT_ONLY",
        "raw�file",
        "CANONICAL",
        "alias",
        "SERIES ONLY",
    ] {
        let encoded: String = query
            .as_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        for sort in ["title", "date_desc", "episode"] {
            for offset in [0, 1, 2, 4, 100] {
                let request = req(&get(&format!("/api/web/library?view=library&kind=video&q={encoded}&sort={sort}&offset={offset}&limit=2"), "QueryParity/1"));
                app.db_pool = pool.clone();
                app.scan_cfg.db_path = Some(db_path.clone());
                app.invalidate_catalog_query_cache();
                let fallback_count = app.runtime_metrics.queries.json()["fallback"]["count"]
                    .as_u64()
                    .unwrap();
                let db_response = app.handle(&request);
                assert_eq!(
                    app.runtime_metrics.queries.json()["fallback"]["count"],
                    fallback_count,
                    "database parity half must execute/materialize SQL without falling back"
                );
                assert_eq!(db_response.status, 200, "query={query:?}, sort={sort}");
                let db_json: serde_json::Value = serde_json::from_slice(&db_response.body).unwrap();
                app.db_pool = None;
                app.scan_cfg.db_path = None;
                app.invalidate_catalog_query_cache();
                let fallback_response = app.handle(&request);
                assert_eq!(fallback_response.status, 200);
                let fallback_json: serde_json::Value =
                    serde_json::from_slice(&fallback_response.body).unwrap();
                assert_eq!(
                    db_json["total"], fallback_json["total"],
                    "query={query:?}, sort={sort}, offset={offset}"
                );
                assert_eq!(
                    db_json["entries"], fallback_json["entries"],
                    "query={query:?}, sort={sort}, offset={offset}"
                );
                if query == "PARENT_ONLY" {
                    assert_eq!(db_json["total"], 0);
                }
                if matches!(query, "été" | "ФиЛьМ") {
                    assert_eq!(
                        db_json["total"], 3,
                        "Unicode case matching must find all three physical files"
                    );
                }
                if query == "raw�file" {
                    assert_eq!(
                        db_json["total"], 1,
                        "decoded lossy basename search must find the byte-preserved path"
                    );
                }

                nonempty += usize::from(db_json["total"].as_u64().unwrap() > 0);
            }
        }
    }
    assert!(nonempty > 100);
}

#[test]
fn timed_out_browser_query_never_runs_memory_fallback() {
    let app = testdata_app();
    let mut control = QueryControl::new(app.runtime_metrics.queries.clone());
    control.deadline = Instant::now();
    let _scope = enter_query_scope(control);
    let response = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video",
        "QueryBudget/1",
    )));
    assert_eq!(response.status, 503);
    let metrics = app.runtime_metrics.queries.json();
    assert_eq!(metrics["fallback"]["count"], 0);
    assert_eq!(metrics["timeouts_total"], 1);
}

#[test]
fn executing_sql_queries_cancel_while_media_status_and_publication_keep_working() {
    let app = testdata_app();
    let pool = app.db_pool.as_ref().unwrap();
    let controls: Vec<_> = (0..4)
        .map(|_| QueryControl::new(app.runtime_metrics.queries.clone()))
        .collect();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let item = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.path.ends_with("tagged.mp4"))
        .unwrap()
        .detail_id;
    std::thread::scope(|scope| {
        let workers: Vec<_> = controls.iter().cloned().map(|control| {
            let started_tx = started_tx.clone();
            scope.spawn(move || {
                let _scope = enter_query_scope(control);
                pool.read(|db| {
                    started_tx.send(()).unwrap();
                    let tx = db.transaction()?;
                    tx.query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n", [], |row| row.get::<_,i64>(0))
                })
            })
        }).collect();
        for _ in 0..4 {
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        assert_eq!(pool.metrics().read_active, 4);
        let start = Instant::now();
        for path in [
            "/health".to_string(),
            "/api/status".to_string(),
            format!("/MediaItems/{item}.mp4"),
        ] {
            let response = app.handle(&req(&get(&path, "QueryMixed/1")));
            assert_eq!(response.status, 200, "{path}");
        }
        assert!(
            start.elapsed() < Duration::from_millis(750),
            "unrelated requests waited for occupied readers"
        );
        // WAL readers must not hold the catalog publication lock or DB writer.
        pool.write(|db| db.set_update_id(1)).unwrap();
        {
            let _catalog = write_recover(&app.catalog);
            app.update_id.store(1, Ordering::Release);
        }
        app.invalidate_catalog_query_cache();
        let stop = Instant::now();
        for control in &controls {
            control.cancellation.cancel();
        }
        for worker in workers {
            assert!(worker.join().unwrap().is_err());
        }
        assert!(stop.elapsed() < Duration::from_millis(300));
    });
    assert!(pool.read(|db| db.get_update_id()).is_ok());
}

#[test]
fn browser_query_catalog_lock_wait_obeys_deadline_during_publication() {
    let app = testdata_app();
    let writer = write_recover(&app.catalog);
    std::thread::scope(|scope| {
        for view in ["library&kind=video", "continue&ids="] {
            let app = &app;
            let worker = scope.spawn(move || {
                let mut control = QueryControl::new(app.runtime_metrics.queries.clone());
                control.deadline = Instant::now() + Duration::from_millis(40);
                let _scope = enter_query_scope(control);
                let start = Instant::now();
                let response = app.handle(&req(&get(
                    &format!("/api/web/library?view={view}"),
                    "QueryPublication/1",
                )));
                assert_eq!(response.status, 503);
                assert!(start.elapsed() < Duration::from_millis(300));
            });
            worker.join().unwrap();
        }
    });
    drop(writer);
}

#[test]
fn browser_fallback_accounts_for_keys_references_and_sort_scratch_together() {
    let mut app = testdata_app();
    app.db_pool = None;
    app.scan_cfg.db_path = None;
    {
        let mut catalog = write_recover(&app.catalog);
        let template = catalog
            .items
            .values()
            .find(|item| item.mime.starts_with("video/"))
            .unwrap()
            .clone();
        catalog.items.clear();
        catalog.by_detail.clear();
        for index in 0..64 {
            let mut item = template.clone();
            item.object_id = format!("budget-{index}");
            item.detail_id = 1_000_000 + index;
            item.device = 42;
            item.inode = index as u64 + 1;
            item.title = format!("{index:04}{}", "x".repeat(124));
            catalog
                .by_detail
                .insert(item.detail_id, item.object_id.clone());
            catalog.items.insert(item.object_id.clone(), item);
        }
    }
    // The retained normalized keys alone fit 12 KiB; the complete set of
    // reference arrays and index-sort scratch must also fit the same budget.
    let request = req(&get(
        "/api/web/library?view=library&kind=video&sort=date_desc&limit=64",
        "QueryScratch/1",
    ));
    for (bytes, expected) in [(16_000, 200), (12_000, 503)] {
        let mut control = QueryControl::new(app.runtime_metrics.queries.clone());
        control.work_byte_limit = bytes;
        let _scope = enter_query_scope(control);
        let response = app.handle(&request);
        assert_eq!(response.status, expected, "scratch budget {bytes}");
        if expected == 503 {
            let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(body["error"]["code"], "catalog_busy");
            assert!(body.get("entries").is_none());
        }
    }
    assert_eq!(
        app.runtime_metrics.queries.json()["budget_exhaustions_total"],
        1
    );
}

#[test]
fn browser_page_cache_reuses_only_the_same_query_page_and_generation() {
    let app = testdata_app();
    let path = app.scan_cfg.db_path.as_deref().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    count_web_media_queries_for_test(path, count.clone());
    let fetch = |suffix: &str| {
        let response = app.handle(&req(&get(
            &format!("/api/web/library?view=library&kind=all&{suffix}"),
            "QueryPageCache/1",
        )));
        assert_eq!(response.status, 200);
        serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()
    };
    let whole = fetch("limit=200");
    let first = fetch("limit=1");
    let second = fetch("limit=1&offset=1");
    assert_eq!(first["total"], whole["total"]);
    assert_eq!(second["total"], whole["total"]);
    assert_eq!(first["entries"][0], whole["entries"][0]);
    assert_eq!(second["entries"][0], whole["entries"][1]);
    assert_eq!(count.load(Ordering::Relaxed), 3);
    assert_eq!(fetch("limit=1"), first);
    assert_eq!(fetch("limit=1&offset=1"), second);
    assert_eq!(
        count.load(Ordering::Relaxed),
        3,
        "identical pages must not execute SQL again"
    );
    fetch("limit=1&sort=date_desc");
    fetch("limit=1&q=Fixture");
    assert_eq!(
        count.load(Ordering::Relaxed),
        5,
        "sort and search form distinct cache keys"
    );
    {
        let _catalog = write_recover(&app.catalog);
        let generation =
            rusty_dlna_protocol::soap::next_system_update_id(app.update_id.load(Ordering::Acquire));
        app.db_pool
            .as_ref()
            .unwrap()
            .write(|db| db.set_update_id(generation))
            .unwrap();
        app.update_id.store(generation, Ordering::Release);
        app.invalidate_catalog_query_cache();
    }
    let updated = fetch("limit=1");
    assert_ne!(updated["generation"], first["generation"]);
    assert_eq!(updated["entries"], first["entries"]);
    assert_eq!(count.load(Ordering::Relaxed), 6);
    stop_counting_web_media_queries_for_test(path);
}

#[test]
fn browser_page_cache_epoch_rejects_inflight_page_when_public_generation_repeats() {
    let app = testdata_app();
    let request = req(&get(
        "/api/web/library?view=library&kind=all&limit=200",
        "QueryCacheEpoch/1",
    ));
    let warm = app.handle(&request);
    assert_eq!(warm.status, 200);
    let before: serde_json::Value = serde_json::from_slice(&warm.body).unwrap();
    let detail_id = before["entries"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let reached = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    crate::web_ui::pause_library_cache_for_test(&app, reached.clone(), release.clone());
    let path = app.scan_cfg.db_path.as_deref().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    count_web_media_queries_for_test(path, count.clone());
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| app.handle(&request));
        reached.wait();
        {
            let mut catalog = write_recover(&app.catalog);
            app.db_pool
                .as_ref()
                .unwrap()
                .write(|db| db.update_detail_title(detail_id, "Fixture Epoch Changed"))
                .unwrap();
            for item in catalog
                .items
                .values_mut()
                .filter(|item| item.detail_id == detail_id)
            {
                item.title = "Fixture Epoch Changed".to_owned();
            }
            // A ui4 wrap can repeat the public value. Publication still replaces
            // the private epoch while this request holds its old cached IDs.
            app.invalidate_catalog_query_cache();
        }
        release.wait();
        let response = worker.join().unwrap();
        assert_eq!(response.status, 200);
        let after: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(after["generation"], before["generation"]);
        let expected_id = detail_id.to_string();
        assert!(after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"].as_str() == Some(expected_id.as_str())
                && entry["title"] == "Fixture Epoch Changed"));
    });
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "an old private epoch must retry SQL even when ui4 is equal"
    );
    stop_counting_web_media_queries_for_test(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_connection_cancels_its_blocking_reader_admission() {
    use tokio::io::AsyncWriteExt;
    let app = Arc::new(testdata_app());
    let pool = app.db_pool.as_ref().unwrap().clone();
    let occupied = Arc::new(std::sync::Barrier::new(5));
    let release = Arc::new(std::sync::Barrier::new(5));
    let holders: Vec<_> = (0..4)
        .map(|_| {
            let pool = pool.clone();
            let occupied = occupied.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                pool.read(|_| {
                    occupied.wait();
                    release.wait();
                    Ok(())
                })
                .unwrap()
            })
        })
        .collect();
    occupied.wait();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handler_app = app.clone();
    let server = tokio::spawn(async move {
        let (socket, peer) = listener.accept().await.unwrap();
        handle_conn(handler_app, socket, peer).await
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(b"GET /api/web/library?view=library&kind=video HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();
    let start = Instant::now();
    while pool.metrics().read_waiters == 0 && start.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let was_waiting = pool.metrics().read_waiters > 0;
    server.abort();
    let _ = server.await;
    let stop = Instant::now();
    while pool.metrics().read_waiters > 0 && stop.elapsed() < Duration::from_millis(300) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let remaining_waiters = pool.metrics().read_waiters;
    release.wait();
    for holder in holders {
        holder.join().unwrap();
    }
    assert!(
        was_waiting,
        "network request never reached reader admission"
    );
    assert_eq!(
        remaining_waiters, 0,
        "aborted async owner left blocking work alive"
    );
    assert!(
        app.runtime_metrics.queries.json()["cancellations_total"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn scratch_budget_is_sticky_and_recorded_once_without_publishing_partial_results() {
    let control = QueryControl::new(Default::default());
    assert_eq!(
        control.check_work_items(MAX_QUERY_WORK_ITEMS + 1),
        Err(QueryStopped::Budget)
    );
    assert_eq!(control.check_work_bytes(0), Err(QueryStopped::Budget));
    control.cancellation.cancel();
    assert_eq!(control.check(), Err(QueryStopped::Budget));
    let bytes = QueryControl::new(Default::default());
    assert_eq!(
        bytes.check_work_bytes(MAX_QUERY_WORK_BYTES + 1),
        Err(QueryStopped::Budget)
    );
    let app = testdata_app();
    let _scope = enter_query_scope(control);
    let response = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video",
        "QueryBudget/1",
    )));
    assert_eq!(response.status, 503);
}

#[test]
fn database_count_hints_require_the_same_transaction_generation() {
    let app = testdata_app();
    let query = |counts| {
        query_db_web_media(
            app.db_pool.as_deref(),
            app.scan_cfg.db_path.as_deref(),
            WebMediaKind::Video,
            "",
            WebMediaSort::Title,
            0,
            2,
            counts,
        )
        .unwrap()
    };
    let actual = query(None);
    assert!(actual.page.total > 0);
    let empty = rusty_dlna_scan::CatalogQueryPage {
        object_ids: Vec::new(),
        total: 0,
        population: 0,
    };
    let stale = query(Some((actual.generation.wrapping_add(1), &empty)));
    assert_eq!(stale.page.total, actual.page.total);
    assert_eq!(stale.page.population, actual.page.population);
    assert_eq!(stale.page.object_ids, actual.page.object_ids);
    let cached = query(Some((actual.generation, &actual.page)));
    assert_eq!(cached.page.total, actual.page.total);
    assert_eq!(cached.page.object_ids, actual.page.object_ids);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_reset_cancels_query_but_fin_only_half_close_still_receives_response() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for reset in [true, false] {
        let app = Arc::new(testdata_app());
        let pool = app.db_pool.as_ref().unwrap().clone();
        let occupied = Arc::new(std::sync::Barrier::new(5));
        let release = Arc::new(std::sync::Barrier::new(5));
        let holders: Vec<_> = (0..4)
            .map(|_| {
                let pool = pool.clone();
                let occupied = occupied.clone();
                let release = release.clone();
                std::thread::spawn(move || {
                    pool.read(|_| {
                        occupied.wait();
                        release.wait();
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect();
        occupied.wait();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler_app = app.clone();
        let server = tokio::spawn(async move {
            let (socket, peer) = listener.accept().await.unwrap();
            handle_conn(handler_app, socket, peer).await
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(b"GET /api/web/library?view=library&kind=video HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();
        let start = Instant::now();
        while pool.metrics().read_waiters == 0 && start.elapsed() < Duration::from_secs(1) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let was_waiting = pool.metrics().read_waiters > 0;
        if reset {
            socket2::SockRef::from(&client)
                .set_linger(Some(Duration::ZERO))
                .unwrap();
            drop(client);
            let start = Instant::now();
            while pool.metrics().read_waiters > 0 && start.elapsed() < Duration::from_millis(300) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let remaining = pool.metrics().read_waiters;
            release.wait();
            for holder in holders {
                holder.join().unwrap();
            }
            assert!(was_waiting);
            assert_eq!(remaining, 0, "RST left blocking SQL admission alive");
            assert!(
                app.runtime_metrics.queries.json()["cancellations_total"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            let _ = tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
        } else {
            client.shutdown().await.unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
            let still_waiting = pool.metrics().read_waiters > 0;
            let cancellations = app.runtime_metrics.queries.json()["cancellations_total"]
                .as_u64()
                .unwrap();
            release.wait();
            for holder in holders {
                holder.join().unwrap();
            }
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            server.await.unwrap().unwrap();
            assert!(
                was_waiting && still_waiting,
                "a valid FIN-only request must keep its query"
            );
            assert_eq!(cancellations, 0);
            assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        }
    }
}

#[test]
fn default_soap_browse_fallback_rejects_excessive_work_before_sorting() {
    let mut app = testdata_app();
    app.db_pool = None;
    app.scan_cfg.db_path = None;
    {
        let mut catalog = write_recover(&app.catalog);
        let item = catalog.items.values().next().unwrap().clone();
        catalog.containers.get_mut("64").unwrap().children =
            vec![item.object_id; MAX_QUERY_WORK_ITEMS + 1];
    }
    let started = Instant::now();
    let (status, xml) = soap_browse(&app, "64", "BrowseDirectChildren", "QueryBudget/1");
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>501</errorCode>"));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(
        app.runtime_metrics.queries.json()["budget_exhaustions_total"],
        1
    );
}

#[test]
fn heavy_catalog_admission_bounds_fallback_and_leaves_light_routes_available() {
    let mut app = testdata_app();
    app.db_pool = None;
    app.scan_cfg.db_path = None;
    let permits: Vec<_> = (0..4)
        .map(|_| {
            app.catalog_query_admission
                .acquire(&QueryControl::new(Default::default()))
                .unwrap()
        })
        .collect();
    let start = Instant::now();
    assert_eq!(
        app.handle(&req(&get("/health", "QueryAdmission/1"))).status,
        200
    );
    assert_eq!(
        app.handle(&req(&get("/api/status", "QueryAdmission/1")))
            .status,
        200
    );
    assert!(start.elapsed() < Duration::from_millis(250));
    {
        let mut control = QueryControl::new(app.runtime_metrics.queries.clone());
        control.deadline = Instant::now() + Duration::from_millis(40);
        let _scope = enter_query_scope(control);
        let response = app.handle(&req(&get(
            "/api/web/library?view=library&kind=video",
            "QueryAdmission/1",
        )));
        assert_eq!(response.status, 503);
        assert_eq!(app.runtime_metrics.queries.json()["fallback"]["count"], 0);
    }
    let control = QueryControl::new(Default::default());
    let cancellation = control.cancellation.clone();
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| app.catalog_query_admission.acquire(&control).is_err());
        cancellation.cancel();
        assert!(waiter.join().unwrap());
    });
    drop(permits);
    assert_eq!(
        app.handle(&req(&get(
            "/api/web/library?view=library&kind=video",
            "QueryAdmission/1"
        )))
        .status,
        200
    );
    assert_eq!(app.runtime_metrics.queries.json()["fallback"]["count"], 1);
}
