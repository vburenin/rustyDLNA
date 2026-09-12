use super::*;
use std::time::Instant;

pub(super) fn folder_app(count: usize) -> App {
    let mut app = testdata_app();
    app.db_pool = None;
    app.scan_cfg.db_path = None;
    let mut catalog = write_recover(&app.catalog);
    let template = catalog
        .items
        .values()
        .find(|item| item.mime.starts_with("video/"))
        .unwrap()
        .clone();
    *catalog = Catalog::new();
    let parent = rusty_dlna_protocol::object_id::BROWSEDIR_ID;
    // A deterministic permutation makes sorting necessary at every size.
    for index in 0..count {
        let mut item = template.clone();
        item.object_id = format!("{parent}${index:X}");
        item.parent_id = parent.into();
        item.detail_id = index as i64 + 1;
        item.inode = index as u64 + 1;
        item.title = format!("Title {:08}", (index * 7919) % count);
        item.path = PathBuf::from(format!("/generated/{}.mkv", item.title));
        item.collection_path = None;
        catalog
            .containers
            .get_mut(parent)
            .unwrap()
            .children
            .push(item.object_id.clone());
        catalog
            .by_detail
            .insert(item.detail_id, item.object_id.clone());
        catalog.items.insert(item.object_id.clone(), item);
    }
    drop(catalog);
    app
}

fn page(app: &App, offset: usize, query: &str) -> HttpResponse {
    app.handle(&req(&get(
        &format!("/api/web/library?view=folders&offset={offset}&limit=200&q={query}"),
        "ScaleTest/1.0",
    )))
}

#[test]
fn folder_pages_keep_order_and_invalidate_after_publication() {
    let app = folder_app(1_000);
    let first = page(&app, 0, "");
    assert_eq!(first.status, 200);
    let first: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&page(&app, 200, "").body).unwrap();
    assert_eq!(first["total"], 1_000);
    assert_eq!(second["total"], first["total"]);
    assert!(
        first["entries"][199]["file_name"].as_str().unwrap()
            < second["entries"][0]["file_name"].as_str().unwrap()
    );
    let deleted = first["entries"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    {
        let mut catalog = write_recover(&app.catalog);
        let id = catalog.by_detail.remove(&deleted).unwrap();
        catalog.items.remove(&id);
        catalog
            .containers
            .get_mut(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
            .unwrap()
            .children
            .retain(|child| child != &id);
        app.update_id.store(1, Ordering::Release);
        app.invalidate_catalog_query_cache();
    }
    let changed: serde_json::Value = serde_json::from_slice(&page(&app, 0, "").body).unwrap();
    assert_eq!(changed["total"], 999);
    assert_eq!(changed["generation"], 1);
    assert_ne!(changed["entries"][0]["id"], first["entries"][0]["id"]);
    let stale = app.handle(&req(&get(
        "/api/web/library?view=folders&generation=0",
        "ScaleTest/1.0",
    )));
    assert_eq!(stale.status, 409);
}

#[test]
fn conditional_folder_request_validates_target_and_skips_projection() {
    let app = folder_app(1_000);
    let response = page(&app, 0, "");
    let etag = response
        .headers
        .iter()
        .find(|(key, _)| key == "ETag")
        .unwrap()
        .1
        .clone();
    for (suffix, expected) in [
        ("", 304),
        ("&folder=missing", 404),
        ("&generation=4294967295", 409),
        ("&limit=0", 400),
    ] {
        let request = req(&format!("GET /api/web/library?view=folders{suffix} HTTP/1.1\r\nHost: 127.0.0.1\r\nIf-None-Match: {etag}\r\n\r\n"));
        assert_eq!(app.handle(&request).status, expected);
    }
}

#[test]
fn folder_projection_rejects_publication_even_when_ui4_generation_repeats() {
    let app = Arc::new(folder_app(1_000));
    let reached = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    web_ui::pause_projection(&app, reached.clone(), release.clone());
    let querying = app.clone();
    let query = std::thread::spawn(move || page(&querying, 0, ""));
    reached.wait();
    {
        let _catalog = write_recover(&app.catalog);
        // A cache clear has its own identity, so ui4 wrap cannot resurrect a
        // projection built before publication, even with the same public value.
        app.invalidate_catalog_query_cache();
    }
    release.wait();
    assert_eq!(query.join().unwrap().status, 409);
    assert_eq!(page(&app, 0, "").status, 200);
}

#[test]
fn cancelled_folder_projection_is_not_published() {
    let app = Arc::new(folder_app(1_000));
    let reached = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let control = QueryControl::new(Default::default());
    let cancellation = control.cancellation.clone();
    web_ui::pause_projection(&app, reached.clone(), release.clone());
    let querying = app.clone();
    let query = std::thread::spawn(move || {
        let _scope = enter_query_scope(control);
        page(&querying, 0, "")
    });
    reached.wait();
    cancellation.cancel();
    release.wait();
    assert_eq!(query.join().unwrap().status, 503);
    assert_eq!(page(&app, 0, "").status, 200);
}

#[test]
#[ignore = "50k/250k generated catalog CPU and paging workload"]
fn folder_paging_benchmark() {
    for count in [50_000, 250_000] {
        let app = Arc::new(folder_app(count));
        for (name, offset, query) in [
            ("first", 0, ""),
            ("deep", 40_000, ""),
            ("search", 0, "Title%20000"),
        ] {
            let mut cold = Vec::new();
            for _ in 0..10 {
                app.invalidate_catalog_query_cache();
                let start = Instant::now();
                assert_eq!(page(&app, offset, query).status, 200);
                cold.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            eprintln!(
                "folder_benchmark count={count} case={name} cache=cold milliseconds={cold:?}"
            );
            let mut samples = Vec::new();
            for _ in 0..10 {
                let start = Instant::now();
                let response = page(&app, offset, query);
                assert_eq!(response.status, 200);
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            eprintln!(
                "folder_benchmark count={count} case={name} cache=warm milliseconds={samples:?}"
            );
        }
        let start = Instant::now();
        let writer_wait = std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(5));
            let writer_barrier = Arc::clone(&barrier);
            let writer_app = &app;
            let writer = scope.spawn(move || {
                writer_barrier.wait();
                let mut waits = Vec::new();
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let started = Instant::now();
                    let guard = write_recover(&writer_app.catalog);
                    waits.push(started.elapsed().as_secs_f64() * 1000.0);
                    drop(guard);
                }
                waits
            });
            for client in 0..4 {
                let app = &app;
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for index in 0..5 {
                        assert_eq!(
                            page(app, 40_000, &format!("{:02}", client * 5 + index)).status,
                            200
                        );
                    }
                });
            }
            writer.join().unwrap()
        });
        eprintln!(
            "folder_benchmark count={count} clients=4 varied_queries=20 wall_ms={}",
            start.elapsed().as_secs_f64() * 1000.0
        );
        eprintln!("folder_benchmark count={count} writer_wait_ms={writer_wait:?}");
    }
}
