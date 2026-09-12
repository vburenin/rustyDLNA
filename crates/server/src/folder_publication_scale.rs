//! Generated folder publication workload; reports stay outside Git.

use super::*;
use std::time::{Duration, Instant};

const ROUNDS: usize = 10;
const LIMIT: usize = 200;
const MARKER_RANKS: [usize; 4] = [0, 1, 40_000, 40_001];

#[derive(Default)]
struct RoundBarrier {
    state: Mutex<(usize, usize)>,
    ready: std::sync::Condvar,
}

impl RoundBarrier {
    fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut state = self.state.lock().unwrap();
        let round = state.0;
        state.1 += 1;
        if state.1 == 6 {
            state.0 += 1;
            state.1 = 0;
            self.ready.notify_all();
            return;
        }
        while state.0 == round {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "workload participant failed or stalled"
            );
            state = self.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

#[derive(Clone, Copy)]
enum WorkloadPage {
    First,
    Deep,
    Prefix(usize),
    Published(u32),
}

impl WorkloadPage {
    fn query(self) -> String {
        match self {
            Self::First | Self::Deep => String::new(),
            Self::Prefix(prefix) => format!("Title {prefix:04}"),
            Self::Published(generation) => format!("Published {generation:02}"),
        }
    }

    fn offset(self) -> usize {
        usize::from(matches!(self, Self::Deep)) * 40_000
    }

    fn request(self, app: &App, generation: Option<u32>) -> HttpResponse {
        let query = self.query().replace(' ', "%20");
        let generation = generation
            .map(|generation| format!("&generation={generation}"))
            .unwrap_or_default();
        app.handle(&req(&get(
            &format!(
                "/api/web/library?view=folders&offset={}&limit={LIMIT}&q={query}{generation}",
                self.offset()
            ),
            "FolderPublicationScale/1.0",
        )))
    }

    fn validate(self, response: &HttpResponse, count: usize, maximum_generation: u32) -> u32 {
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let generation = body["generation"].as_u64().unwrap() as u32;
        assert!(generation <= maximum_generation);
        let (total, expected_ranks): (usize, Vec<usize>) = match self {
            Self::First | Self::Deep => (count, (self.offset()..count).take(LIMIT).collect()),
            Self::Prefix(prefix) => {
                let start = prefix * 10_000;
                let end = start.saturating_add(10_000).min(count);
                (
                    end.saturating_sub(start),
                    (start..end).take(LIMIT).collect(),
                )
            }
            Self::Published(expected) if generation == expected => (4, MARKER_RANKS.to_vec()),
            Self::Published(_) => (0, Vec::new()),
        };
        assert_eq!(body["total"].as_u64().unwrap() as usize, total);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), expected_ranks.len());
        for (entry, rank) in entries.iter().zip(expected_ranks) {
            let detail = entry["id"].as_str().unwrap().parse::<usize>().unwrap();
            assert_eq!(
                ((detail - 1) * 7919) % count,
                rank,
                "ordered ID in generation {generation}"
            );
            assert_eq!(entry["file_name"], format!("Title {rank:08}.mkv"));
            let title = if generation > 0 && MARKER_RANKS.contains(&rank) {
                format!("Published {generation:02} marker")
            } else {
                format!("Title {rank:08}")
            };
            assert_eq!(
                entry["title"], title,
                "metadata must match response generation {generation}"
            );
        }
        generation
    }
}

// Generate patches through the real scanner journal, without creating a second
// large database or reaching into CatalogPatch's private representation.
fn publication_patches(
    app: &App,
    count: usize,
    tree: &TestTree,
) -> Vec<rusty_dlna_scan::CatalogPatch> {
    let path = tree.path().join("patches.db");
    let db = LibraryDb::open(&path).unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    let catalog = read_recover(&app.catalog);
    let mut details = Vec::new();
    for index in 0..count {
        if !MARKER_RANKS.contains(&((index * 7919) % count)) {
            continue;
        }
        let item = &catalog.items[&format!("64${index:X}")];
        raw.execute(
            "INSERT INTO DETAILS(ID, PATH, SIZE, TIMESTAMP, TITLE, DATE, MIME, DEVICE, INODE)
             VALUES (?1, ?2, 1, 1, ?3, '2026-01-01', 'video/x-matroska', 1, ?4)",
            rusqlite::params![
                item.detail_id,
                item.path.to_str().unwrap(),
                item.title,
                i64::try_from(item.inode).unwrap()
            ],
        )
        .unwrap();
        db.upsert_object(
            &item.object_id,
            &item.parent_id,
            &item.class,
            Some(item.detail_id),
            &item.title,
            None,
        )
        .unwrap();
        details.push(item.detail_id);
    }
    drop(catalog);
    assert_eq!(details.len(), 4);
    (1..=ROUNDS)
        .map(|generation| {
            db.begin_catalog_change_capture().unwrap();
            let transaction = db.transaction().unwrap();
            for detail in &details {
                db.update_detail_title(*detail, &format!("Published {generation:02} marker"))
                    .unwrap();
            }
            transaction.commit().unwrap();
            db.load_catalog_patch().unwrap()
        })
        .collect()
}

#[test]
#[ignore = "50k/250k four-client folder pages during actual incremental publication"]
fn folder_pages_under_incremental_publication_benchmark() {
    for count in [50_000, 250_000] {
        let mut app = super::large_library::folder_app(count);
        app.update_id.store(0, Ordering::Release);
        let tree = TestTree::new("folder-publication-scale");
        let patches = publication_patches(&app, count, &tree);
        let database = tree.path().join("patches.db");
        app.db_pool = Some(Arc::new(DbPool::open(&database, 2).unwrap()));
        app.scan_cfg.db_path = Some(database);
        let app = Arc::new(app);
        // These negative results must be discarded as their generations become
        // visible. This exercises invalidation of a previously empty projection.
        for generation in 1..=ROUNDS as u32 {
            let case = WorkloadPage::Published(generation);
            case.validate(&case.request(&app, Some(0)), count, 0);
        }
        let barrier = Arc::new(RoundBarrier::default());
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started = Instant::now();
        std::thread::scope(|scope| {
            let publication_app = Arc::clone(&app);
            let publication_barrier = Arc::clone(&barrier);
            let publication_active = Arc::clone(&active);
            let publisher = scope.spawn(move || {
                let mut samples = Vec::new();
                for (round, patch) in patches.into_iter().enumerate() {
                    publication_barrier.wait();
                    std::thread::sleep(Duration::from_millis(2));
                    let active_clients = publication_active.load(Ordering::Acquire);
                    let start = Instant::now();
                    apply_catalog_update(
                        &publication_app,
                        CatalogUpdate::Patch(patch),
                        ScanDelta { changed: 4, ..ScanDelta::default() },
                        "generated folder publication workload",
                    ).unwrap();
                    let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
                    let generation = round as u32 + 1;
                    assert_eq!(publication_app.update_id.load(Ordering::Acquire), generation);
                    let case = WorkloadPage::Published(generation);
                    let stale = case.request(&publication_app, Some(generation - 1));
                    assert_eq!(stale.status, 409);
                    // A retry without the stale generation must show all four
                    // updated rows, despite the initially cached empty result.
                    assert_eq!(case.validate(&case.request(&publication_app, None), count, generation), generation);
                    samples.push(serde_json::json!({"generation": generation, "milliseconds_including_wait": milliseconds, "active_clients": active_clients}));
                    publication_barrier.wait();
                }
                assert!(samples.iter().any(|sample| sample["active_clients"].as_u64().unwrap() > 0));
                eprintln!("folder_publication count={count} publication_samples={samples:?}");
            });
            let mut clients = Vec::new();
            for client in 0..4 {
                let app = Arc::clone(&app);
                let barrier = Arc::clone(&barrier);
                let active = Arc::clone(&active);
                clients.push(scope.spawn(move || {
                    let mut samples = Vec::new();
                    for round in 0..ROUNDS {
                        let case = match client {
                            0 => WorkloadPage::First,
                            1 => WorkloadPage::Deep,
                            2 => WorkloadPage::Prefix(round % (count / 10_000)),
                            _ => WorkloadPage::Published(round as u32 + 1),
                        };
                        barrier.wait();
                        let mut completed = false;
                        for attempt in 0..8 {
                            let requested = app.update_id.load(Ordering::Acquire);
                            active.fetch_add(1, Ordering::AcqRel);
                            let start = Instant::now();
                            let response = case.request(&app, Some(requested));
                            let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
                            active.fetch_sub(1, Ordering::AcqRel);
                            samples.push(serde_json::json!({"round": round, "attempt": attempt, "status": response.status, "milliseconds": milliseconds}));
                            match response.status {
                                200 => {
                                    assert_eq!(case.validate(&response, count, round as u32 + 1), requested);
                                    completed = true;
                                    break;
                                }
                                409 => std::thread::yield_now(),
                                status => panic!("unexpected page status {status}: {}", String::from_utf8_lossy(&response.body)),
                            }
                        }
                        assert!(completed, "publication must permit a bounded successful retry");
                        barrier.wait();
                    }
                    eprintln!("folder_publication count={count} client={client} page_samples={samples:?}");
                }));
            }
            let health_app = Arc::clone(&app);
            let health_barrier = Arc::clone(&barrier);
            let health = scope.spawn(move || {
                let mut samples = Vec::new();
                for round in 0..ROUNDS {
                    health_barrier.wait();
                    for path in ["/health", "/api/status"] {
                        let start = Instant::now();
                        let response = health_app.handle(&req(&get(path, "FolderPublicationScale/1.0")));
                        let elapsed = start.elapsed();
                        assert_eq!(response.status, 200, "{path}: {}", String::from_utf8_lossy(&response.body));
                        assert!(elapsed < Duration::from_secs(5), "{path} exceeded the query deadline under load");
                        samples.push(serde_json::json!({"round": round, "path": path, "milliseconds": elapsed.as_secs_f64() * 1000.0}));
                    }
                    health_barrier.wait();
                }
                eprintln!("folder_publication count={count} health_status_samples={samples:?}");
            });
            publisher.join().unwrap();
            for client in clients {
                client.join().unwrap();
            }
            health.join().unwrap();
        });
        eprintln!(
            "folder_publication count={count} clients=4 rounds={ROUNDS} wall_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}
