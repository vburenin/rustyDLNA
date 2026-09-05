use super::*;
use crate::tests::{stored_probe_fields, TempPath};
use crate::*;

#[test]
fn distinct_hardlink_sidecars_share_one_raw_probe_and_fill_pass() {
    let tmp = TempPath::new("probe-sidecar-hardlinks");
    std::fs::create_dir_all(&*tmp).unwrap();
    let first = tmp.join("first.mkv");
    let second = tmp.join("second.mkv");
    write_fake_mkv(&first, 64);
    std::fs::hard_link(&first, &second).unwrap();
    std::fs::write(tmp.join("first.probe.toml"), "video=\"hevc\"\n").unwrap();
    std::fs::write(tmp.join("second.probe.toml"), "video=\"h264\"\n").unwrap();
    let db_path = tmp.join("files.db");
    let gate = std::sync::Arc::new(HelperGate::new(2, 8));
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        helper_gate: Some(gate.clone()),
        ..Default::default()
    };
    scan(&cfg).unwrap();
    assert_eq!(gate.metrics().admitted_total, 1, "one raw physical probe");
    assert_eq!(stored_probe_fields(&db_path, &first).1, "hevc");
    assert_eq!(stored_probe_fields(&db_path, &second).1, "h264");

    let first_sidecar = tmp.join("first.probe.toml");
    let second_sidecar = tmp.join("second.probe.toml");
    std::fs::write(&first_sidecar, "video=\"hevc\"\nhdr=\"dv-p8\"\n").unwrap();
    std::fs::write(&second_sidecar, "video=\"h264\"\nhdr=\"sdr\"\n").unwrap();
    let before = gate.metrics().admitted_total;
    monitor_dirty(&cfg, &[first_sidecar.clone(), second_sidecar.clone()]).unwrap();
    assert_eq!(
        gate.metrics().admitted_total - before,
        1,
        "one settled multi-sidecar batch must share the raw physical probe"
    );
    assert_eq!(stored_probe_fields(&db_path, &first).2, "dv-p8");
    assert_eq!(stored_probe_fields(&db_path, &second).2, "sdr");

    std::fs::write(&first_sidecar, "video=\"hevc\"\nhdr=\"dv-p7\"\n").unwrap();
    std::fs::write(&second_sidecar, "video=\"h264\"\nhdr=\"dv-p8\"\n").unwrap();
    let before = gate.metrics().admitted_total;
    monitor(&cfg).unwrap();
    assert_eq!(
        gate.metrics().admitted_total - before,
        1,
        "periodic reconciliation must batch distinct overlays for one physical file"
    );
    assert_eq!(stored_probe_fields(&db_path, &first).2, "dv-p7");
    assert_eq!(stored_probe_fields(&db_path, &second).2, "dv-p8");

    let db = LibraryDb::open(&db_path).unwrap();
    db.connection()
        .execute("UPDATE DETAILS SET PROBE_SIDECAR_FINGERPRINT = NULL", [])
        .unwrap();
    drop(db);
    let before = gate.metrics().admitted_total;
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session.prepare_fill_missing_av_meta().unwrap();
    session.publish(prepared).unwrap();
    assert_eq!(
        gate.metrics().admitted_total - before,
        1,
        "standalone fill must share one raw probe across provenance groups"
    );
    assert_eq!(stored_probe_fields(&db_path, &first).1, "hevc");
    assert_eq!(stored_probe_fields(&db_path, &second).1, "h264");
    assert_ne!(
        stored_probe_fields(&db_path, &first).3,
        stored_probe_fields(&db_path, &second).3
    );

    let before = gate.metrics().admitted_total;
    let prepared = session.prepare_fill_missing_av_meta().unwrap();
    let (update, delta) = session.publish(prepared).unwrap().into_parts();
    assert!(update.is_none());
    assert_eq!(delta, ScanDelta::default());
    assert_eq!(gate.metrics().admitted_total, before);
}

#[test]
fn staged_rebuild_merges_ids_and_hydrates_the_current_live_bookmark_atomically() {
    let tmp = TempPath::new("scan-session-merge");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let initial = scan(&cfg).unwrap();
    let initial_item = initial
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap();
    let detail_id = initial_item.detail_id;
    let object_id = initial_item.object_id.clone();
    let initial_object_row_id: i64 = open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .connection()
        .query_row(
            "SELECT ID FROM OBJECTS WHERE OBJECT_ID=?1",
            [&object_id],
            |row| row.get(0),
        )
        .unwrap();

    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session.prepare_rebuild_objects().unwrap();
    let staged_object_row_id: i64 = session
        .stage
        .as_ref()
        .unwrap()
        .connection()
        .query_row(
            "SELECT ID FROM OBJECTS WHERE OBJECT_ID=?1",
            [&object_id],
            |row| row.get(0),
        )
        .unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.update_bookmark(detail_id, Some(321), Some(4)).unwrap();
    live.set_update_id(41).unwrap();
    let published = session.publish_to_db(&live, prepared, Some(42)).unwrap();
    let (update, _) = published.into_parts();
    let Some(CatalogUpdate::Replacement(catalog)) = update else {
        panic!("rebuild must publish a replacement catalog");
    };
    let item = catalog.items.get(&object_id).unwrap();
    assert_eq!(item.detail_id, detail_id);
    assert_eq!((item.bookmark_sec, item.watch_count), (321, 4));
    assert_eq!(live.get_bookmark(detail_id).unwrap(), Some((321, 4)));
    assert_eq!(live.get_update_id().unwrap(), 42);
    assert_eq!(
        live.find_detail_by_path(&path_to_db(&media))
            .unwrap()
            .unwrap()
            .id,
        detail_id
    );
    let published_object_row_id: i64 = live
        .connection()
        .query_row(
            "SELECT ID FROM OBJECTS WHERE OBJECT_ID=?1",
            [&object_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(published_object_row_id, staged_object_row_id);
    assert_ne!(
        staged_object_row_id, initial_object_row_id,
        "the regression must exercise an explicit rebuilt OBJECTS.ID"
    );
}

#[test]
fn staged_detail_merge_round_trips_every_persisted_field() {
    let tmp = TempPath::new("scan-session-detail-fields");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let mut session = ScanSession::new(&cfg).unwrap();
    let token = session.begin_prepare().unwrap();
    let stage = session.stage.as_ref().unwrap();
    stage
        .connection()
        .execute(
            "UPDATE DETAILS SET
                PATH='raw-path', SIZE=101, TIMESTAMP=102, TITLE='title', DURATION='103',
                BITRATE=104, SAMPLERATE=105, CREATOR='creator', ARTIST='artist',
                ALBUM_ARTIST='album-artist', COMPOSER='composer', CONTRIBUTOR='contributor',
                ALBUM='album', GENRE='genre', OUTLINE='outline', PLOT='plot', COMMENT='comment', CHANNELS=106,
                DISC=107, TRACK=108, RATING=109, DATE='2026-08-21', RESOLUTION='1920x1080',
                THUMBNAIL=1, ALBUM_ART=NULL, ROTATION=110, DLNA_PN='pn', MIME='video/test',
                DEVICE=111, INODE=112, CONTAINER='matroska', VIDEO='hevc', AUDIO='truehd',
                AUDIO_STREAMS='streams', HDR='dv-p7', STREAM_PROBE_REV=113,
                PROBE_SIDECAR_FINGERPRINT='v2:distinct-stage-fingerprint'
             WHERE ID=?1",
            [detail_id],
        )
        .unwrap();
    const FIELDS: &str = "ID, PATH, SIZE, TIMESTAMP, TITLE, DURATION, BITRATE, SAMPLERATE,
        CREATOR, ARTIST, ALBUM_ARTIST, COMPOSER, CONTRIBUTOR, ALBUM, GENRE, OUTLINE, PLOT, COMMENT,
        CHANNELS, DISC, TRACK, RATING, DATE, RESOLUTION, THUMBNAIL, ALBUM_ART, ROTATION,
        DLNA_PN, MIME, DEVICE, INODE, CONTAINER, VIDEO, AUDIO, AUDIO_STREAMS, HDR,
        STREAM_PROBE_REV, PROBE_SIDECAR_FINGERPRINT";
    let read_fields = |db: &LibraryDb| {
        db.connection()
            .query_row(
                &format!("SELECT {FIELDS} FROM DETAILS WHERE ID=?1"),
                [detail_id],
                |row| {
                    (0..38)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                },
            )
            .unwrap()
    };
    let expected = read_fields(stage);
    let prepared = session.prepared(token, None, ScanDelta::default(), None, Vec::new());
    session.publish(prepared).unwrap();
    let reopened = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(read_fields(&reopened), expected);
}

#[test]
fn reusable_stage_publishes_expiry_of_a_live_only_bookmark() {
    let tmp = TempPath::new("scan-session-live-bookmark-expiry");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        bookmark_retention_days: 1,
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let mut catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let mut session = ScanSession::new(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.update_bookmark(detail_id, Some(120), Some(2)).unwrap();
    live.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![unix_now_seconds() - 2 * 86_400, detail_id],
        )
        .unwrap();
    for item in catalog.items.values_mut() {
        if item.detail_id == detail_id {
            item.bookmark_sec = 120;
            item.watch_count = 2;
        }
    }
    live.set_update_id(20).unwrap();

    let prepared = session.prepare_monitor(&[], true).unwrap();
    let published = session.publish_to_db(&live, prepared, Some(21)).unwrap();
    let (update, _) = published.into_parts();
    let Some(CatalogUpdate::Patch(patch)) = update else {
        panic!("the actual live bookmark deletion must produce a patch");
    };
    catalog.apply_patch(patch);
    let item = catalog.get_item_by_detail(detail_id).unwrap();
    assert_eq!((item.bookmark_sec, item.watch_count), (0, 0));
    assert_eq!(live.get_bookmark(detail_id).unwrap(), None);
    assert_eq!(live.get_update_id().unwrap(), 21);
}

#[test]
fn reusable_stage_does_not_publish_a_bookmark_refreshed_after_backup() {
    let tmp = TempPath::new("scan-session-refreshed-bookmark");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        bookmark_retention_days: 1,
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.update_bookmark(detail_id, Some(90), Some(1)).unwrap();
    live.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![unix_now_seconds() - 2 * 86_400, detail_id],
        )
        .unwrap();
    live.set_update_id(30).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    live.update_bookmark(detail_id, Some(180), Some(3)).unwrap();

    let prepared = session.prepare_monitor(&[], true).unwrap();
    let published = session.publish_to_db(&live, prepared, Some(31)).unwrap();
    let (update, _) = published.into_parts();
    assert!(
        update.is_none(),
        "a stage-side stale deletion must not publish after the live bookmark was refreshed"
    );
    assert_eq!(live.get_bookmark(detail_id).unwrap(), Some((180, 3)));
    assert_eq!(live.get_update_id().unwrap(), 30);
}

#[test]
fn no_change_preparation_defers_stale_art_cleanup_until_publication() {
    let tmp = TempPath::new("scan-session-stale-art");
    let media_dir = tmp.join("media");
    let db_dir = tmp.join("database");
    let art_dir = db_dir.join("art");
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::create_dir_all(&art_dir).unwrap();
    let db_path = db_dir.join("files.db");
    let stale_art = art_dir.join("stale.jpg");
    std::fs::write(&stale_art, b"stale cache sentinel").unwrap();
    let live = LibraryDb::open(&db_path).unwrap();
    live.upsert_album_art(&path_to_db(&stale_art)).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(db_path),
        thumbnails: false,
        ..Default::default()
    };

    let mut dropped_session = ScanSession::new(&cfg).unwrap();
    let dropped = dropped_session.prepare_monitor(&[], true).unwrap();
    assert!(
        dropped.update.is_none(),
        "only stale private art was pruned"
    );
    assert_eq!(dropped.stale_art, vec![path_to_db(&stale_art)]);
    assert!(
        stale_art.is_file(),
        "private preparation must not delete a live cache file"
    );
    drop(dropped);
    drop(dropped_session);
    assert!(
        stale_art.is_file(),
        "dropping unpublished work must leave the live cache untouched"
    );

    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session.prepare_monitor(&[], true).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    let mut published = session.publish_to_db(&live, prepared, None).unwrap();
    session.finish_publication();
    assert!(
        stale_art.is_file(),
        "prepared publication still defers filesystem cleanup"
    );
    published.cleanup_stale_art();
    assert!(
        !stale_art.exists(),
        "deferred cleanup removes the now-unreferenced cache file"
    );

    std::fs::write(&stale_art, b"stale cache sentinel").unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.upsert_album_art(&path_to_db(&stale_art)).unwrap();
    let mut direct = ScanSession::new(&cfg).unwrap();
    let prepared = direct.prepare_monitor(&[], true).unwrap();
    let _published = direct.publish(prepared).unwrap();
    assert!(
        !stale_art.exists(),
        "one-shot publication completes serialized cache cleanup before returning"
    );
}

#[test]
fn unchanged_full_reconciliation_with_playlist_is_generation_neutral() {
    let tmp = TempPath::new("scan-session-playlist-noop");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    std::fs::write(media_dir.join("Queue.m3u"), "movie.mkv\n").unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.set_update_id(41).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let object_sequence = |db: &LibraryDb| {
        db.connection()
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='OBJECTS'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let initial_sequence = object_sequence(&live);
    assert_eq!(
        object_sequence(session.stage.as_ref().unwrap()),
        initial_sequence
    );

    for candidate_update_id in [u32::MAX, 0] {
        let prepared = session.prepare_monitor(&[], true).unwrap();
        assert_eq!(
            object_sequence(session.stage.as_ref().unwrap()),
            initial_sequence,
            "no-op stage object upserts must not consume AUTOINCREMENT IDs"
        );
        let mut published = session
            .publish_to_db(&live, prepared, Some(candidate_update_id))
            .unwrap();
        assert_eq!(published.committed_update_id(), None);
        let (update, delta) = published.take_parts();
        assert!(
            update.is_none(),
            "equal playlist/item rows must not create a patch"
        );
        assert_eq!(delta, ScanDelta::default());
        assert_eq!(live.get_update_id().unwrap(), 41);
        assert_eq!(
            object_sequence(&live),
            initial_sequence,
            "a no-op stage sequence must not inflate the live sequence"
        );
        session.finish_publication();
    }

    let token = session.begin_prepare().unwrap();
    let stage = session.stage.as_ref().unwrap();
    stage
        .upsert_object(
            "sequence-regression-object",
            "0",
            "container.storageFolder",
            None,
            "Sequence regression object",
            None,
        )
        .unwrap();
    let staged_id: i64 = stage
        .connection()
        .query_row(
            "SELECT ID FROM OBJECTS WHERE OBJECT_ID='sequence-regression-object'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(staged_id, initial_sequence + 1);
    assert_eq!(object_sequence(stage), initial_sequence + 1);
    let prepared = session.prepared(token, None, ScanDelta::default(), None, Vec::new());
    let mut published = session.publish_to_db(&live, prepared, Some(42)).unwrap();
    assert_eq!(published.committed_update_id(), Some(42));
    assert!(matches!(
        published.take_parts().0,
        Some(CatalogUpdate::Patch(_))
    ));
    let live_id: i64 = live
        .connection()
        .query_row(
            "SELECT ID FROM OBJECTS WHERE OBJECT_ID='sequence-regression-object'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live_id, initial_sequence + 1);
    assert_eq!(object_sequence(&live), initial_sequence + 1);
}

#[test]
fn staged_publication_rejects_a_nonsequential_update_id_and_rolls_back() {
    let tmp = TempPath::new("scan-session-update-id-rollback");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.set_update_id(41).unwrap();
    let base_path = media_dir.join("base.mkv");
    let base_detail_id = live
        .find_detail_by_path(&path_to_db(&base_path))
        .unwrap()
        .unwrap()
        .id;
    live.update_bookmark(base_detail_id, Some(120), Some(2))
        .unwrap();
    let epoch_before = live.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap();
    let catalog_row_counts = |database: &LibraryDb| {
        database
            .connection()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM OBJECTS),
                    (SELECT COUNT(*) FROM DETAILS),
                    (SELECT COUNT(*) FROM ALBUM_ART),
                    (SELECT COUNT(*) FROM CAPTIONS),
                    (SELECT COUNT(*) FROM PLAYLISTS),
                    (SELECT COUNT(*) FROM PLAYLIST_ITEMS)",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .unwrap()
    };
    let catalog_rows_before = catalog_row_counts(&live);
    let bookmark_before = live.get_bookmark(base_detail_id).unwrap();
    let added = media_dir.join("added.mkv");
    write_fake_mkv(&added, 64);

    for rejected in [41, 43] {
        let mut session = ScanSession::new(&cfg).unwrap();
        let prepared = session
            .prepare_monitor(std::slice::from_ref(&added), true)
            .unwrap();
        let error = match session.publish_to_db(&live, prepared, Some(rejected)) {
            Ok(_) => panic!("nonsequential update ID {rejected} was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ScanError::Invariant(ref message) if message.contains("expected 42")),
            "{error}"
        );
        assert_eq!(live.get_update_id().unwrap(), 41);
        assert_eq!(
            live.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap(),
            epoch_before
        );
        assert_eq!(live.get_bookmark(base_detail_id).unwrap(), bookmark_before);
        assert_eq!(catalog_row_counts(&live), catalog_rows_before);
        assert!(live
            .find_detail_by_path(&path_to_db(&added))
            .unwrap()
            .is_none());
    }

    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    let published = session.publish_to_db(&live, prepared, Some(42)).unwrap();
    assert_eq!(published.committed_update_id(), Some(42));
    assert_eq!(live.get_update_id().unwrap(), 42);
    assert_eq!(live.get_bookmark(base_detail_id).unwrap(), bookmark_before);
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_some());
}

#[test]
fn staged_publication_accepts_wrapped_next_update_id() {
    let tmp = TempPath::new("scan-session-update-id-wrap");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.set_update_id(u32::MAX).unwrap();
    let added = media_dir.join("wrapped.mkv");
    write_fake_mkv(&added, 64);
    let epoch_before = live.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();

    let error = match session.publish_to_db(&live, prepared, Some(1)) {
        Ok(_) => panic!("nonsequential post-wrap update ID was accepted"),
        Err(error) => error,
    };
    assert!(
        matches!(error, ScanError::Invariant(ref message) if message.contains("expected 0")),
        "{error}"
    );
    assert_eq!(live.get_update_id().unwrap(), u32::MAX);
    assert_eq!(
        live.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap(),
        epoch_before
    );
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_none());

    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    let published = session.publish_to_db(&live, prepared, Some(0)).unwrap();
    assert_eq!(published.committed_update_id(), Some(0));
    assert_eq!(live.get_update_id().unwrap(), 0);
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_some());
}

#[test]
fn staged_case_only_detail_change_merges_with_binary_equality() {
    let tmp = TempPath::new("scan-session-case-only-merge");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let mut session = ScanSession::new(&cfg).unwrap();
    let token = session.begin_prepare().unwrap();
    session
        .stage
        .as_ref()
        .unwrap()
        .connection()
        .execute("UPDATE DETAILS SET TITLE='MOVIE' WHERE ID=?1", [detail_id])
        .unwrap();
    let prepared = session.prepared(token, None, ScanDelta::default(), None, Vec::new());
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.connection()
        .execute("UPDATE DETAILS SET TITLE='movie' WHERE ID=?1", [detail_id])
        .unwrap();
    live.set_update_id(51).unwrap();

    let mut published = session.publish_to_db(&live, prepared, Some(52)).unwrap();
    assert_eq!(published.committed_update_id(), Some(52));
    assert!(matches!(
        published.take_parts().0,
        Some(CatalogUpdate::Patch(_))
    ));
    let title: String = live
        .connection()
        .query_row(
            "SELECT TITLE FROM DETAILS WHERE ID=?1",
            [detail_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "MOVIE");
}

#[test]
fn reusable_scan_session_keeps_one_file_journals_bounded_without_rebackup() {
    let tmp = TempPath::new("scan-session-targeted");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    for index in 0..128 {
        write_fake_mkv(&media_dir.join(format!("old-{index:03}.mkv")), 64);
    }
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    assert_eq!(session.backup_count(), 1);

    let first = media_dir.join("new-first.mkv");
    write_fake_mkv(&first, 64);
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&first), true)
        .unwrap();
    assert_eq!(
        session
            .stage
            .as_ref()
            .unwrap()
            .catalog_change_counts()
            .unwrap(),
        (0, 0, 0),
        "incremental patch capture must be dropped before later replacement preparation"
    );
    let counts = session
        .stage
        .as_ref()
        .unwrap()
        .scan_change_counts()
        .unwrap();
    assert_eq!(counts[0], 1, "only the new detail belongs in the journal");
    assert!(
        counts[1] <= 12,
        "one new item must not journal the existing object tree: {counts:?}"
    );
    assert_eq!(counts[4], 0, "targeted media adds do not resync playlists");
    assert_eq!(counts[5], 0, "unchanged settings must not be journaled");
    session.publish(prepared).unwrap();

    let second = media_dir.join("new-second.mkv");
    write_fake_mkv(&second, 64);
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&second), true)
        .unwrap();
    assert_eq!(
        session.backup_count(),
        1,
        "a successful targeted event keeps the stage reusable"
    );
    session.publish(prepared).unwrap();
}

#[test]
fn stale_or_dropped_prepared_work_rebacks_up_before_the_next_merge() {
    let tmp = TempPath::new("scan-session-epoch");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let mut first_session = ScanSession::new(&cfg).unwrap();
    let mut stale_session = ScanSession::new(&cfg).unwrap();
    let first = media_dir.join("first.mkv");
    let stale = media_dir.join("stale.mkv");
    write_fake_mkv(&first, 64);
    write_fake_mkv(&stale, 64);
    let first_prepared = first_session
        .prepare_monitor(std::slice::from_ref(&first), true)
        .unwrap();
    let stale_prepared = stale_session
        .prepare_monitor(std::slice::from_ref(&stale), true)
        .unwrap();
    first_session.publish(first_prepared).unwrap();
    assert!(matches!(
        stale_session.publish(stale_prepared),
        Err(ScanError::Invariant(message)) if message.contains("epoch")
    ));
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    assert!(live
        .find_detail_by_path(&path_to_db(&first))
        .unwrap()
        .is_some());
    assert!(live
        .find_detail_by_path(&path_to_db(&stale))
        .unwrap()
        .is_none());

    let retry = stale_session
        .prepare_monitor(std::slice::from_ref(&stale), true)
        .unwrap();
    assert_eq!(stale_session.backup_count(), 2);
    stale_session.publish(retry).unwrap();
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&stale))
        .unwrap()
        .is_some());

    let dropped = media_dir.join("dropped.mkv");
    write_fake_mkv(&dropped, 64);
    drop(
        stale_session
            .prepare_monitor(std::slice::from_ref(&dropped), true)
            .unwrap(),
    );
    let retry = stale_session
        .prepare_monitor(std::slice::from_ref(&dropped), true)
        .unwrap();
    assert_eq!(stale_session.backup_count(), 3);
    stale_session.publish(retry).unwrap();
}

#[test]
fn prepared_work_is_bound_to_its_originating_session() {
    let tmp = TempPath::new("scan-session-owner");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let foreign_path = media_dir.join("foreign.mkv");
    let other_path = media_dir.join("other.mkv");
    let owner_path = media_dir.join("owner-retry.mkv");
    write_fake_mkv(&foreign_path, 64);
    write_fake_mkv(&other_path, 64);
    write_fake_mkv(&owner_path, 64);
    let mut owner = ScanSession::new(&cfg).unwrap();
    let mut other = ScanSession::new(&cfg).unwrap();
    let prepared = owner
        .prepare_monitor(std::slice::from_ref(&foreign_path), true)
        .unwrap();
    let other_prepared = other
        .prepare_monitor(std::slice::from_ref(&other_path), true)
        .unwrap();
    assert!(matches!(
        other.publish(prepared),
        Err(ScanError::Invariant(message)) if message.contains("no longer belongs")
    ));
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&foreign_path))
        .unwrap()
        .is_none());
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&other_path))
        .unwrap()
        .is_none());
    drop(other_prepared);

    let retry = owner
        .prepare_monitor(std::slice::from_ref(&owner_path), true)
        .unwrap();
    assert_eq!(
        owner.backup_count(),
        2,
        "the source session was invalidated"
    );
    owner.publish(retry).unwrap();
}

#[test]
fn prepared_work_rejects_a_different_live_database() {
    let tmp = TempPath::new("scan-session-wrong-db");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let added = media_dir.join("added.mkv");
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let other_path = tmp.join("other.db");
    let other = open_library_db(&other_path).unwrap();
    let other_before = other.detail_count().unwrap();
    write_fake_mkv(&added, 64);
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    assert!(matches!(
        session.publish_to_db(&other, prepared, None),
        Err(ScanError::Invariant(message)) if message.contains("different live database")
    ));
    assert_eq!(other.detail_count().unwrap(), other_before);
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_none());
}

#[test]
fn committed_detach_failure_keeps_stage_reusable_and_next_attach_recovers() {
    let tmp = TempPath::new("scan-session-detach-recovery");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let first = media_dir.join("first.mkv");
    write_fake_mkv(&first, 64);
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&first), true)
        .unwrap();
    live.fail_next_scan_stage_detach();
    let published = session.publish_to_db(&live, prepared, None).unwrap();
    assert!(published.writer_reopen_required());
    session.finish_publication();
    assert!(live
        .find_detail_by_path(&path_to_db(&first))
        .unwrap()
        .is_some());

    let second = media_dir.join("second.mkv");
    write_fake_mkv(&second, 64);
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&second), true)
        .unwrap();
    assert_eq!(session.backup_count(), 1);
    let published = session.publish_to_db(&live, prepared, None).unwrap();
    assert!(!published.writer_reopen_required());
    session.finish_publication();
    assert!(live
        .find_detail_by_path(&path_to_db(&second))
        .unwrap()
        .is_some());
}

#[test]
fn live_change_capture_ends_before_later_bookmark_writes() {
    let tmp = TempPath::new("scan-session-live-capture-end");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session.prepare_monitor(&[], true).unwrap();
    session.publish_to_db(&live, prepared, None).unwrap();
    session.finish_publication();
    assert_eq!(live.catalog_change_counts().unwrap(), (0, 0, 0));
    live.update_bookmark(detail_id, Some(120), Some(1)).unwrap();
    assert_eq!(
        live.catalog_change_counts().unwrap(),
        (0, 0, 0),
        "bookmark requests must not feed a scanner TEMP journal"
    );
}

#[test]
fn attach_failure_invalidates_prepared_work_and_rebacks_up() {
    let tmp = TempPath::new("scan-session-attach-failure");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let added = media_dir.join("added.mkv");
    write_fake_mkv(&added, 64);
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    live.fail_next_scan_stage_attach();
    assert!(session.publish_to_db(&live, prepared, None).is_err());
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_none());

    let retry = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    assert_eq!(session.backup_count(), 2);
    session.publish_to_db(&live, retry, None).unwrap();
    session.finish_publication();
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_some());
}

#[test]
fn scan_session_cleanup_removes_only_unlocked_stage_families() {
    let tmp = TempPath::new("scan-stage-cleanup");
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join("files.db");
    open_library_db(&db_path).unwrap();
    let prefix = scan_stage_prefix(&db_path);
    let stale = tmp.join(format!("{prefix}4294967295-9"));
    let stale_wal = path_with_suffix(&stale, "-wal");
    let stale_shm = path_with_suffix(&stale, "-shm");
    let stale_journal = path_with_suffix(&stale, "-journal");
    let stale_lock = path_with_suffix(&stale, ".lock");
    let active = tmp.join(format!("{prefix}{}-77", std::process::id()));
    let active_lock_path = path_with_suffix(&active, ".lock");
    let unrelated = tmp.join(format!("{prefix}not-a-pid-9.lock"));
    let symlink_base = tmp.join(format!("{prefix}123-88"));
    let symlink_lock = path_with_suffix(&symlink_base, ".lock");
    let symlink_target = tmp.join("unrelated-lock-target");
    for path in [
        &stale,
        &stale_wal,
        &stale_shm,
        &stale_journal,
        &stale_lock,
        &active,
        &active_lock_path,
        &unrelated,
        &symlink_base,
        &symlink_target,
    ] {
        std::fs::write(path, b"sentinel").unwrap();
    }
    std::os::unix::fs::symlink(&symlink_target, &symlink_lock).unwrap();
    let active_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&active_lock_path)
        .unwrap();
    assert!(try_lock_scan_stage(&active_lock).unwrap());
    let cfg = ScanConfig {
        db_path: Some(db_path),
        ..Default::default()
    };
    let session = ScanSession::new(&cfg).unwrap();
    assert!(!stale.exists());
    assert!(!stale_wal.exists());
    assert!(!stale_shm.exists());
    assert!(!stale_journal.exists());
    assert!(!stale_lock.exists());
    assert!(active.exists());
    assert!(active_lock_path.exists());
    assert!(unrelated.exists());
    assert!(symlink_base.exists());
    assert!(symlink_lock.is_symlink());
    assert_eq!(std::fs::read(&symlink_target).unwrap(), b"sentinel");
    drop(session);
    unlock_scan_stage(&active_lock);
}

#[test]
fn stale_cleanup_rejects_an_unlinked_lock_inode_replaced_after_flock() {
    let tmp = TempPath::new("scan-stage-cleanup-aba");
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join("files.db");
    open_library_db(&db_path).unwrap();
    let stale = tmp.join(format!("{}4294967295-19", scan_stage_prefix(&db_path)));
    let stale_lock = path_with_suffix(&stale, ".lock");
    std::fs::write(&stale, b"old-stage").unwrap();
    std::fs::write(&stale_lock, b"old-lock").unwrap();
    let replacement = std::sync::Arc::new(std::sync::Mutex::new(None));
    CLEANUP_PREEMPTIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(stale_lock.clone(), std::sync::Arc::clone(&replacement));
    let cfg = ScanConfig {
        db_path: Some(db_path),
        ..Default::default()
    };

    let session = ScanSession::new(&cfg).unwrap();
    let held = replacement
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("cleanup preemption installed a replacement family");
    assert_eq!(held.stage_path, stale);
    assert_eq!(held.lock_path, stale_lock);
    assert_eq!(
        std::fs::read(&held.stage_path).unwrap(),
        b"replacement-stage-sentinel"
    );
    assert!(held.lock_path.is_file());
    drop(session);
    assert_eq!(
        std::fs::read(&held.stage_path).unwrap(),
        b"replacement-stage-sentinel"
    );
}

#[test]
fn stage_reservation_retries_when_its_created_lock_path_is_preempted() {
    let tmp = TempPath::new("scan-stage-reservation-aba");
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join("files.db");
    open_library_db(&db_path).unwrap();
    let replacement = std::sync::Arc::new(std::sync::Mutex::new(None));
    RESERVATION_PREEMPTIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(db_path.clone(), std::sync::Arc::clone(&replacement));
    let cfg = ScanConfig {
        db_path: Some(db_path),
        ..Default::default()
    };

    let session = ScanSession::new(&cfg).unwrap();
    let held = replacement
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("reservation preemption installed a replacement family");
    assert_ne!(session.stage_path, held.stage_path);
    assert_ne!(session.stage_lock_path, held.lock_path);
    assert_eq!(
        std::fs::read(&held.stage_path).unwrap(),
        b"replacement-stage-sentinel"
    );
    drop(session);
    assert_eq!(
        std::fs::read(&held.stage_path).unwrap(),
        b"replacement-stage-sentinel"
    );
}

#[test]
fn scan_session_preserves_non_utf_database_directories_through_publish_and_cleanup() {
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempPath::new("scan-session-non-utf-db");
    let media_dir = tmp.join("video");
    let db_dir = tmp.join(std::ffi::OsString::from_vec(b"database-\xff".to_vec()));
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::create_dir_all(&db_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(db_dir.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let stale = db_dir.join(format!(
        "{}999999-1",
        scan_stage_prefix(&cfg.db_path.clone().unwrap())
    ));
    let stale_lock = path_with_suffix(&stale, ".lock");
    for path in [
        &stale,
        &path_with_suffix(&stale, "-wal"),
        &path_with_suffix(&stale, "-shm"),
        &path_with_suffix(&stale, "-journal"),
        &stale_lock,
    ] {
        std::fs::write(path, b"stale").unwrap();
    }
    let added = media_dir.join("added.mkv");
    write_fake_mkv(&added, 64);
    let mut session = ScanSession::new(&cfg).unwrap();
    assert!(!stale.exists() && !stale_lock.exists());
    let stage_path = session.stage_path.clone();
    let lock_path = session.stage_lock_path.clone();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    session.publish(prepared).unwrap();
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_some());
    drop(session);
    for path in [
        stage_path.clone(),
        path_with_suffix(&stage_path, "-wal"),
        path_with_suffix(&stage_path, "-shm"),
        path_with_suffix(&stage_path, "-journal"),
        lock_path,
    ] {
        assert!(
            !path.exists(),
            "stage artifact survived: {}",
            path.display()
        );
    }
}

#[test]
fn scan_session_stage_name_does_not_extend_a_long_database_filename() {
    let tmp = TempPath::new("scan-session-long-db-name");
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join(format!("{}.db", "catalog".repeat(30)));
    assert!(db_path.file_name().unwrap().as_encoded_bytes().len() > 200);
    open_library_db(&db_path).unwrap();
    let cfg = ScanConfig {
        db_path: Some(db_path),
        ..Default::default()
    };
    let session = ScanSession::new(&cfg).unwrap();
    assert!(
        session
            .stage_path
            .file_name()
            .unwrap()
            .as_encoded_bytes()
            .len()
            < 96
    );
}

#[test]
fn direct_forget_advances_epoch_and_rejects_an_older_prepared_rebuild() {
    let tmp = TempPath::new("scan-session-forget-epoch");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media_dir],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session.prepare_rebuild_objects().unwrap();
    assert_eq!(forget_path(&cfg, &media).unwrap(), 1);
    assert!(matches!(
        session.publish(prepared),
        Err(ScanError::Invariant(message)) if message.contains("epoch")
    ));
    assert!(open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .find_detail_by_path(&path_to_db(&media))
        .unwrap()
        .is_none());
}

#[test]
fn exhausted_scan_epoch_fails_before_merging_any_rows() {
    let tmp = TempPath::new("scan-session-epoch-max");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("base.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let added = media_dir.join("added.mkv");
    write_fake_mkv(&added, 64);
    let mut session = ScanSession::new(&cfg).unwrap();
    let prepared = session
        .prepare_monitor(std::slice::from_ref(&added), true)
        .unwrap();
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.set_scan_catalog_epoch(u64::MAX).unwrap();
    session
        .stage
        .as_ref()
        .unwrap()
        .set_scan_catalog_epoch(u64::MAX)
        .unwrap();
    assert!(matches!(
        session.publish_to_db(&live, prepared, None),
        Err(ScanError::Invariant(message)) if message.contains("epoch space")
    ));
    assert!(live
        .find_detail_by_path(&path_to_db(&added))
        .unwrap()
        .is_none());
}

#[test]
fn cancelled_or_failed_bookmark_snapshot_leaves_live_publication_unchanged() {
    let tmp = TempPath::new("scan-session-publication-rollback");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let base = media_dir.join("base.mkv");
    write_fake_mkv(&base, 64);
    let cancellation = CancellationToken::default();
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        cancellation: cancellation.clone(),
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == base)
        .unwrap()
        .detail_id;
    let live = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    live.set_update_id(70).unwrap();

    let cancelled_path = media_dir.join("cancelled.mkv");
    write_fake_mkv(&cancelled_path, 64);
    let mut cancelled_session = ScanSession::new(&cfg).unwrap();
    let cancelled = cancelled_session
        .prepare_monitor(std::slice::from_ref(&cancelled_path), true)
        .unwrap();
    cancellation.cancel();
    assert!(matches!(
        cancelled_session.publish_to_db(&live, cancelled, Some(71)),
        Err(ScanError::Cancelled)
    ));
    assert!(live
        .find_detail_by_path(&path_to_db(&cancelled_path))
        .unwrap()
        .is_none());
    assert_eq!(live.get_update_id().unwrap(), 70);

    let fresh_cfg = ScanConfig {
        cancellation: CancellationToken::default(),
        ..cfg
    };
    let failed_path = media_dir.join("snapshot-failed.mkv");
    write_fake_mkv(&failed_path, 64);
    let mut failed_session = ScanSession::new(&fresh_cfg).unwrap();
    let failed = failed_session.prepare_scan(false).unwrap();
    live.update_bookmark(detail_id, Some(120), Some(1)).unwrap();
    live.connection()
        .execute(
            "UPDATE BOOKMARKS SET SEC='invalid' WHERE ID=?1",
            [detail_id],
        )
        .unwrap();
    assert!(failed_session
        .publish_to_db(&live, failed, Some(71))
        .is_err());
    assert!(live
        .find_detail_by_path(&path_to_db(&failed_path))
        .unwrap()
        .is_none());
    assert_eq!(live.get_update_id().unwrap(), 70);
    let stored: String = live
        .connection()
        .query_row(
            "SELECT SEC FROM BOOKMARKS WHERE ID=?1",
            [detail_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, "invalid");
}

#[test]
fn blocked_fill_probe_never_holds_the_live_writer() {
    let tmp = TempPath::new("scan-session-fill-writer");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let db_path = tmp.join("files.db");
    let initial_cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&initial_cfg).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap()
        .detail_id;
    let gate = std::sync::Arc::new(HelperGate::new(1, 1));
    let cfg = ScanConfig {
        helper_gate: Some(std::sync::Arc::clone(&gate)),
        helper_queue_timeout: std::time::Duration::from_secs(5),
        ..initial_cfg
    };
    let mut session = ScanSession::new(&cfg).unwrap();
    session
        .stage
        .as_ref()
        .unwrap()
        .connection()
        .execute(
            "UPDATE DETAILS SET STREAM_PROBE_REV=0 WHERE ID=?1",
            [detail_id],
        )
        .unwrap();
    let held = gate.try_acquire().unwrap();
    let worker = std::thread::spawn(move || session.prepare_fill_missing_av_meta());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while gate.metrics().queued_total == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "fill probe did not queue"
        );
        std::thread::yield_now();
    }
    let live = open_library_db(&db_path).unwrap();
    let started = std::time::Instant::now();
    let transaction = live.transaction().unwrap();
    live.update_bookmark(detail_id, Some(77), Some(2)).unwrap();
    live.set_update_id(88).unwrap();
    transaction.commit().unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a blocked fill probe held the live writer for {:?}",
        started.elapsed()
    );
    drop(held);
    let _prepared = worker.join().unwrap().unwrap();
    assert_eq!(live.get_bookmark(detail_id).unwrap(), Some((77, 2)));
    assert_eq!(live.get_update_id().unwrap(), 88);
}
