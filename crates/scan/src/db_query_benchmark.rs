//! Opt-in, fixed-workload SQL profiles. The ASCII baseline retains the query
//! shape measured before bundle E, solely to compare VM work and latency.
use super::*;
use std::time::Instant;

const BEFORE_CTE: &str = r"WITH matching AS (
 SELECT MIN(d.ID) AS detail_id FROM DETAILS d
 WHERE web_media_kind(COALESCE(d.MIME, '')) = 'item.videoItem' AND (
 LOWER(COALESCE(d.TITLE, '')) LIKE ?1 ESCAPE '\' OR
 LOWER(COALESCE(d.ARTIST, '')) LIKE ?1 ESCAPE '\' OR
 LOWER(COALESCE(d.ALBUM_ARTIST, '')) LIKE ?1 ESCAPE '\' OR
 LOWER(COALESCE(d.ALBUM, '')) LIKE ?1 ESCAPE '\' OR
 LOWER(COALESCE(d.PATH, '')) LIKE ?1 ESCAPE '\')
 GROUP BY CASE WHEN COALESCE(d.INODE, 0) = 0 THEN 'id:' || d.ID ELSE d.DEVICE || ':' || d.INODE END
 ), representatives AS (
 SELECT m.detail_id, COALESCE(MIN(CASE WHEN o.REF_ID IS NULL THEN o.OBJECT_ID END), MIN(o.OBJECT_ID)) AS object_id
 FROM matching m JOIN OBJECTS o ON o.DETAIL_ID = m.detail_id GROUP BY m.detail_id) ";

fn before_page(db: &LibraryDb, query: &str, offset: usize) -> rusqlite::Result<Vec<String>> {
    let _: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM OBJECTS WHERE OBJECT_ID <> '0'",
        [],
        |r| r.get(0),
    )?;
    let needle = format!("%{query}%");
    let total: i64 = db
        .conn
        .prepare_cached(&format!("{BEFORE_CTE}SELECT COUNT(*) FROM representatives"))?
        .query_row([&needle], |r| r.get(0))?;
    if sqlite_page_value(offset) >= total {
        return Ok(Vec::new());
    }
    db.conn.prepare_cached(&format!("{BEFORE_CTE}SELECT r.object_id FROM representatives r JOIN DETAILS d ON d.ID=r.detail_id ORDER BY web_media_title_key(COALESCE(d.COLLECTION_PATH,d.PATH,''), COALESCE(d.MIME,''), COALESCE(NULLIF(d.TITLE,''),d.PATH,'')), d.ID LIMIT 200 OFFSET {offset}"))?
        .query_map([needle], |r| r.get(0))?.collect()
}

fn thread_cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
#[ignore = "50k/250k SQL comparison and VM profile; run --release --ignored --nocapture"]
fn browser_query_scale_profile() {
    eprintln!("SQLite={} profile=release rows=50000,250000 samples=5; cold=first statement preparation; warm=remaining samples; elapsed is wall ms, VM measured separately", rusqlite::version());
    for size in [50_000i64, 250_000] {
        let db = LibraryDb::open_memory().unwrap();
        let tx = db.transaction().unwrap();
        {
            let mut detail = tx.prepare("INSERT INTO DETAILS(ID,PATH,TITLE,MIME,DEVICE,INODE,DATE) VALUES (?1,?2,?3,'video/mp4',1,?1,'2026-01-01')").unwrap();
            let mut object = tx.prepare("INSERT INTO OBJECTS(OBJECT_ID,PARENT_ID,CLASS,DETAIL_ID,NAME) VALUES (?1,'64','item.videoItem',?2,?3)").unwrap();
            for id in 1..=size {
                let title = format!("Title {id:06}");
                detail
                    .execute(params![id, format!("/media/{title}.mp4"), title])
                    .unwrap();
                object
                    .execute(params![format!("64${id}"), id, title])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        for (query, offset) in [
            ("", 0),
            ("", 40_000),
            ("title 04", 0),
            ("title 2", 0),
            ("missing", 0),
        ] {
            let expected = before_page(&db, query, offset).unwrap();
            let actual = db
                .query_web_media_page(WebMediaKind::Video, query, WebMediaSort::Title, offset, 200)
                .unwrap();
            assert_eq!(expected, actual.object_ids);
            for version in ["before", "after", "after-counts"] {
                db.conn.flush_prepared_statement_cache();
                let mut samples = Vec::new();
                let mut cpu_ms = Vec::new();
                for _ in 0..5 {
                    let cpu_start = thread_cpu_ns();
                    let start = Instant::now();
                    if version == "before" {
                        before_page(&db, query, offset).unwrap();
                    } else if version == "after-counts" {
                        db.query_web_media_page_with_counts(
                            WebMediaKind::Video,
                            query,
                            WebMediaSort::Title,
                            offset,
                            200,
                            Some(CatalogQueryCounts {
                                total: actual.total,
                                population: actual.population,
                            }),
                        )
                        .unwrap();
                    } else {
                        db.query_web_media_page(
                            WebMediaKind::Video,
                            query,
                            WebMediaSort::Title,
                            offset,
                            200,
                        )
                        .unwrap();
                    }
                    samples.push(start.elapsed().as_secs_f64() * 1000.0);
                    cpu_ms.push(thread_cpu_ns().saturating_sub(cpu_start) as f64 / 1_000_000.0);
                }
                let steps = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let counter = steps.clone();
                db.conn
                    .progress_handler(
                        1,
                        Some(move || {
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            false
                        }),
                    )
                    .unwrap();
                if version == "before" {
                    before_page(&db, query, offset).unwrap();
                } else if version == "after-counts" {
                    db.query_web_media_page_with_counts(
                        WebMediaKind::Video,
                        query,
                        WebMediaSort::Title,
                        offset,
                        200,
                        Some(CatalogQueryCounts {
                            total: actual.total,
                            population: actual.population,
                        }),
                    )
                    .unwrap();
                } else {
                    db.query_web_media_page(
                        WebMediaKind::Video,
                        query,
                        WebMediaSort::Title,
                        offset,
                        200,
                    )
                    .unwrap();
                }
                db.clear_query_control().unwrap();
                eprintln!("rows={size} query={query:?} offset={offset} version={version} wall_ms={samples:?} thread_cpu_ms={cpu_ms:?} vm_steps={}",steps.load(std::sync::atomic::Ordering::Relaxed));
            }
        }
        for version in ["before", "after"] {
            let (cte, order) = web_media_query_sql(WebMediaKind::Video, WebMediaSort::Title);
            let (sql, values) = if version == "before" {
                (format!("EXPLAIN QUERY PLAN {BEFORE_CTE}SELECT r.object_id FROM representatives r JOIN DETAILS d ON d.ID=r.detail_id ORDER BY d.TITLE,d.ID LIMIT 200 OFFSET 40000"), vec![Value::Text("%title%".into())])
            } else {
                (format!("EXPLAIN QUERY PLAN {cte}SELECT r.object_id FROM representatives r JOIN DETAILS d ON d.ID=r.detail_id JOIN OBJECTS o ON o.OBJECT_ID=r.object_id ORDER BY {order} LIMIT ?2 OFFSET ?3"), vec![Value::Text("title".into()),Value::Integer(200),Value::Integer(40000)])
            };
            let plan = db
                .conn
                .prepare(&sql)
                .unwrap()
                .query_map(params_from_iter(values), |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            eprintln!("rows={size} version={version} explain={plan:?}");
        }
    }
}
