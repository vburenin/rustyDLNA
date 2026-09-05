use super::*;
use rusty_dlna_scan::scan;
use rusty_dlna_soap::xml_tag_text;

const TINY_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08,
    0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12,
    0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20,
    0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27,
    0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x2A, 0x1F, 0xFF, 0xD9,
];

fn jpeg_declaring_dimensions(width: u16, height: u16) -> Vec<u8> {
    let [height_high, height_low] = height.to_be_bytes();
    let [width_high, width_low] = width.to_be_bytes();
    vec![
        0xff,
        0xd8,
        0xff,
        0xc0,
        0x00,
        0x0b,
        0x08,
        height_high,
        height_low,
        width_high,
        width_low,
        0x01,
        0x01,
        0x11,
        0x00,
        0xff,
        0xd9,
    ]
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn browser_ai_upscale_is_descriptor_backed_and_exactly_sdr_gated() {
    let mut app = testdata_app();
    app.cfg.web.encoder = "h264_nvenc".into();
    let shader_path = app.cache_dir.join("test-upscale.glsl");
    std::fs::write(
        &shader_path,
        b"//!HOOK LUMA\n//!BIND HOOKED\n//!SAVE AI_OUT\nvec4 hook(){return HOOKED_tex(HOOKED_pos);}\n",
    )
    .unwrap();
    app.ai_upscale_profiles.push(BrowserAiUpscaleProfile {
        name: "fsrcnnx-16".into(),
        shader_file: Arc::new(std::fs::File::open(&shader_path).unwrap()),
        shader_sha256: "c".repeat(64),
        max_source_width: 1920,
        max_source_height: 1080,
        max_source_pixels_per_second: 52_000_000,
    });
    let tagged = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.path.ends_with("tagged.mp4"))
        .cloned()
        .unwrap();
    {
        let mut catalog = write_recover(&app.catalog);
        let object_id = catalog.by_detail[&tagged.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.video = "h264".into();
        item.probe.hdr = "sdr".into();
        item.probe.bit_depth = 8;
        item.probe.width = 1280;
        item.probe.height = 720;
        item.probe.frame_rate = "24000/1001".into();
    }

    let upscaled = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=full_hd&video_mode=transcode&audio_mode=transcode",
            tagged.detail_id
        ),
        "Browser/1.0",
    )));
    let upscaled = upscaled.remux_job.expect("AI upscale browser job");
    assert!(upscaled.ai_upscale_shader_file.is_some());
    assert!(upscaled
        .cache_key
        .contains("browser-ai-upscale-libplacebo-v1"));
    assert!(upscaled.args.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        argument.contains("custom_shader_path=/proc/self/fd/5")
            && argument.contains("w=1920:h=1080")
            && !argument.contains("min(iw")
    }));
    assert!(upscaled
        .fallback_args
        .as_ref()
        .is_some_and(|args| args.iter().any(|argument| argument
            .to_string_lossy()
            .contains("custom_shader_path=/proc/self/fd/5"))));

    {
        let mut catalog = write_recover(&app.catalog);
        let object_id = catalog.by_detail[&tagged.detail_id].clone();
        catalog.items.get_mut(&object_id).unwrap().probe.hdr = "hdr10".into();
    }
    let hdr = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=full_hd&video_mode=transcode&audio_mode=transcode",
            tagged.detail_id
        ),
        "Browser/1.0",
    )));
    let hdr = hdr.remux_job.expect("ordinary HDR browser job");
    assert!(hdr.ai_upscale_shader_file.is_none());
    assert!(!hdr.cache_key.contains("browser-ai-upscale-libplacebo-v1"));
    assert!(hdr.args.iter().any(|argument| argument
        .to_string_lossy()
        .contains("libplacebo=apply_dolbyvision=true")));

    {
        let mut catalog = write_recover(&app.catalog);
        let object_id = catalog.by_detail[&tagged.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.hdr = "sdr".into();
        item.probe.bit_depth = 10;
    }
    let ten_bit_sdr = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=full_hd&video_mode=transcode&audio_mode=transcode",
            tagged.detail_id
        ),
        "Browser/1.0",
    )));
    assert!(ten_bit_sdr
        .remux_job
        .expect("ordinary 10-bit SDR browser job")
        .ai_upscale_shader_file
        .is_none());

    {
        let mut catalog = write_recover(&app.catalog);
        let object_id = catalog.by_detail[&tagged.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.bit_depth = 8;
        item.probe.frame_rate = "60000/1001".into();
    }
    let over_rate = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=full_hd&video_mode=transcode&audio_mode=transcode",
            tagged.detail_id
        ),
        "Browser/1.0",
    )));
    assert!(over_rate
        .remux_job
        .expect("ordinary over-envelope browser job")
        .ai_upscale_shader_file
        .is_none());
}

fn require_fixture_library_at(library: &Path) {
    for relative in [
        "music/song.flac",
        "music/song.mp3",
        "music/song.nfo",
        "music/fixture.m3u8",
        "pictures/shot.jpg",
        "video/movie.mkv",
        "video/movie.nfo",
        "video/movie.srt",
        "video/movie.en.srt",
        "video/movie-poster.jpg",
        "video/tagged.mp4",
        "video/dvp7.mkv",
        "video/Movie.2024.2160p.UHD.BDRemux.HDR.DV.HEVC.mkv",
        "video/The Show/tvshow.nfo",
        "video/The Show/S01E01.mkv",
        "video/The Show/S01E01.nfo",
        "video/The Show/S01E02.mkv",
        "video/The Show/S01E02.nfo",
        "sample/ignored.mkv",
        "@eaDir/junk.mkv",
        "exclude_me/secret.mkv",
        "unfinished.mkv.part",
    ] {
        let path = library.join(relative);
        assert!(
                path.is_file(),
                "required tracked fixture is missing: {}; restore testdata instead of generating it during tests",
                path.display()
            );
    }
}

fn require_fixture_library() {
    require_fixture_library_at(&workspace().join("testdata/library"));
}

fn testdata_app() -> App {
    let root = workspace();
    let test_tree = TestTree::new("fixtures");
    require_fixture_library();
    let cfg = Config {
        friendly_name: "rustyDLNA-test".into(),
        media_dir: vec!["testdata/library".into()],
        exclude_dir: vec!["exclude_me".into()],
        cache_dir: Some(test_tree.path().join("cache").display().to_string()),
        db_dir: Some(test_tree.path().join("database").display().to_string()),
        rescan_secs: 0,
        remap: rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
name = "crkey-dvp7"
client = "CrKey"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
        )
        .unwrap(),
        transcode: TranscodeCfg {
            enable: true,
            encoder: "libx264".into(),
            max_jobs: 1,
            ..TranscodeCfg::default()
        },
        ..Config::default()
    };
    let mut app = App::from_config(cfg, 18200, 11900, &root);
    let cat = scan(&app.scan_cfg).unwrap();
    *write_recover(&app.catalog) = cat;
    app.test_tree = Some(test_tree);
    app
}

#[test]
#[should_panic(expected = "required tracked fixture is missing")]
fn fixture_setup_fails_instead_of_generating_missing_inputs() {
    let missing = TestTree::new("missing-fixtures");
    require_fixture_library_at(missing.path());
}

fn req(raw: &str) -> HttpRequest {
    HttpRequest::parse_headers(raw).unwrap()
}

fn get(path: &str, ua: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\n\r\n")
}

fn resp_header<'a>(r: &'a HttpResponse, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
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

#[test]
fn test_ports_do_not_collide_with_live() {
    assert!(!collides_with_live_ports(18200, 11900));
    assert!(collides_with_live_ports(8200, 11900));
    assert!(collides_with_live_ports(18200, 1900));

    let _: fn(u16) -> u16 = resolve_http_port;
    let _: fn() -> u16 = resolve_ssdp_port;
}

#[test]
fn scan_policy_config_defaults_aliases_and_ranges_are_validated() {
    let parsed: Config = toml::from_str(
        r#"
enable_subtitles = false
enable_thumbnail = false
enable_thumbnail_filmstrip = true
thumbnail_width = 640
thumbnail_quality = 5
scan_command_timeout_secs = 9
scan_workers = 16
helper_max_jobs = 8
helper_queue_capacity = 32
helper_queue_timeout_secs = 7
shutdown_timeout_secs = 12
rescan_secs = 30
rescan_max_secs = 900
bookmark_retention_days = 90
album_art_names = ["AlbumArt.jpg", "{stem}-cover.png"]
"#,
    )
    .unwrap();
    assert!(!parsed.subtitles);
    assert!(!parsed.thumbnails);
    assert!(parsed.thumbnail_filmstrip);
    assert_eq!(parsed.thumbnail_width, 640);
    assert_eq!(Config::default().derived_image_quality, 8);
    assert_eq!(parsed.scan_workers, 16);
    assert_eq!(parsed.helper_max_jobs, 8);
    assert_eq!(parsed.helper_queue_capacity, 32);
    assert_eq!(parsed.helper_queue_timeout_secs, 7);
    assert_eq!(parsed.shutdown_timeout_secs, 12);
    assert_eq!(parsed.rescan_secs, 30);
    assert_eq!(parsed.rescan_max_secs, 900);
    assert_eq!(parsed.bookmark_retention_days, 90);
    assert!(validate_http_config(&parsed).is_ok());

    let mut invalid = Config {
        thumbnail_width: 0,
        ..Config::default()
    };
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("thumbnail_width"));
    invalid = Config::default();
    invalid.album_art_names = vec!["../outside.jpg".into()];
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("album_art_names"));
    invalid = Config::default();
    invalid.scan_command_timeout_secs = 0;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("scan_command_timeout_secs"));
    invalid = Config::default();
    invalid.scan_workers = 0;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("scan_workers"));
    invalid.scan_workers = 65;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("scan_workers"));
    invalid = Config::default();
    invalid.helper_max_jobs = 0;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("helper_max_jobs"));
    invalid = Config::default();
    invalid.helper_queue_capacity = 0;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("helper_queue_capacity"));
    invalid = Config::default();
    invalid.helper_queue_timeout_secs = 0;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("helper_queue_timeout_secs"));
    invalid = Config::default();
    invalid.shutdown_timeout_secs = 1;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("shutdown_timeout_secs"));
    invalid = Config::default();
    invalid.rescan_max_secs = invalid.rescan_secs - 1;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("rescan_max_secs"));
    invalid = Config::default();
    invalid.bookmark_retention_days = 36_501;
    assert!(validate_http_config(&invalid)
        .unwrap_err()
        .to_string()
        .contains("bookmark_retention_days"));
    assert!(toml::from_str::<Config>("friendy_name = \"typo\"")
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
    assert!(toml::from_str::<Config>("[transcode]\nenabel = true")
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
    assert!(toml::from_str::<Config>("[web]\nenabel = true")
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
    assert!(
        !toml::from_str::<Config>("[web]\nenable = false")
            .unwrap()
            .web
            .enable
    );
    let mut invalid_web_encoder = Config::default();
    invalid_web_encoder.web.encoder = "hevc_nvenc".into();
    assert!(validate_http_config(&invalid_web_encoder)
        .unwrap_err()
        .to_string()
        .contains("web.encoder"));
    let mut invalid_ai_jobs = Config::default();
    invalid_ai_jobs.web.ai_upscale_max_jobs = 0;
    assert!(validate_http_config(&invalid_ai_jobs)
        .unwrap_err()
        .to_string()
        .contains("ai_upscale_max_jobs"));
    let ai_profile = || WebAiUpscaleCfg {
        name: "fsrcnnx-16".into(),
        shader_path: "models/fsrcnnx-16.glsl".into(),
        max_source_width: 1920,
        max_source_height: 1080,
        max_source_pixels_per_second: 52_000_000,
    };
    let mut invalid_ai_encoder = Config::default();
    invalid_ai_encoder.transcode.enable = true;
    invalid_ai_encoder.web.ai_upscale.push(ai_profile());
    assert!(validate_http_config(&invalid_ai_encoder)
        .unwrap_err()
        .to_string()
        .contains("web.ai_upscale requires"));
    let mut duplicate_ai_names = Config::default();
    duplicate_ai_names.transcode.enable = true;
    duplicate_ai_names.web.encoder = "h264_nvenc".into();
    duplicate_ai_names.web.ai_upscale = vec![ai_profile(), ai_profile()];
    assert!(validate_http_config(&duplicate_ai_names)
        .unwrap_err()
        .to_string()
        .contains("duplicated"));
    let mut invalid_ai_rate = Config::default();
    invalid_ai_rate.transcode.enable = true;
    invalid_ai_rate.web.encoder = "h264_nvenc".into();
    let mut profile = ai_profile();
    profile.max_source_pixels_per_second = 999_999;
    invalid_ai_rate.web.ai_upscale.push(profile);
    assert!(validate_http_config(&invalid_ai_rate)
        .unwrap_err()
        .to_string()
        .contains("max_source_pixels_per_second"));
    let parsed_ai: Config = toml::from_str(
        r#"
[web]
encoder = "h264_nvenc"
ai_upscale_max_jobs = 1

[[web.ai_upscale]]
name = "quality"
shader_path = "models/quality.glsl"
max_source_width = 1920
max_source_height = 1080
max_source_pixels_per_second = 52000000

[transcode]
enable = true
"#,
    )
    .unwrap();
    assert!(validate_http_config(&parsed_ai).is_ok());
    assert!(toml::from_str::<Config>("[[remap]]\nacton = \"original\"")
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
}

#[test]
fn transcode_cache_maintenance_limits_are_validated_when_transcoding_is_disabled() {
    for (cache_max_mb, cache_max_age_days, expected) in [
        (0, 30, "transcode.cache_max_mb"),
        (1_048_577, 30, "transcode.cache_max_mb"),
        (512, 0, "transcode.cache_max_age_days"),
        (512, 36_501, "transcode.cache_max_age_days"),
    ] {
        let mut invalid = Config::default();
        assert!(!invalid.transcode.enable);
        invalid.transcode.cache_max_mb = cache_max_mb;
        invalid.transcode.cache_max_age_days = cache_max_age_days;
        let error = validate_http_config(&invalid).unwrap_err().to_string();
        assert!(error.contains(expected), "unexpected error: {error}");
    }
}

#[test]
fn adaptive_reconcile_cadence_backs_off_and_resets() {
    let unchanged = ReconcileOutcome {
        success: true,
        changed: false,
    };
    let changed = ReconcileOutcome {
        success: true,
        changed: true,
    };
    let failed = ReconcileOutcome {
        success: false,
        changed: false,
    };

    assert_eq!(
        next_reconcile_interval_secs(30, 900, 30, unchanged, Duration::from_secs(1)),
        60
    );
    assert_eq!(
        next_reconcile_interval_secs(30, 900, 60, unchanged, Duration::from_secs(20)),
        400,
        "a costly full walk should keep its duty cycle at or below five percent"
    );
    assert_eq!(
        next_reconcile_interval_secs(30, 900, 800, unchanged, Duration::from_secs(100)),
        900
    );
    assert_eq!(
        next_reconcile_interval_secs(30, 900, 900, changed, Duration::from_secs(1)),
        30
    );
    assert_eq!(
        next_reconcile_interval_secs(30, 900, 900, failed, Duration::from_secs(1)),
        30
    );
    assert_eq!(
        next_reconcile_interval_secs(30, 30, 30, unchanged, Duration::from_secs(100)),
        30,
        "rescan_max_secs=0 is resolved to the fixed minimum before selection"
    );
}

#[test]
fn catalog_query_cache_is_generation_scoped_and_bounded() {
    let mut cache = CatalogQueryCache::default();
    let page = rusty_dlna_scan::CatalogQueryPage {
        object_ids: vec!["2$8$1".into()],
        total: 1,
        population: 1,
    };
    for index in 0..(CATALOG_QUERY_CACHE_ENTRIES + 20) {
        cache.insert(7, format!("page-{index}"), page.clone());
    }
    assert_eq!(cache.entries.len(), CATALOG_QUERY_CACHE_ENTRIES);
    assert!(cache.get(7, "page-0").is_none());
    assert!(cache
        .get(7, &format!("page-{}", CATALOG_QUERY_CACHE_ENTRIES + 19))
        .is_some());
    assert!(cache.get(8, "page-275").is_none());
    assert!(cache.entries.is_empty(), "old generations are discarded");

    cache.insert(8, "x".repeat(CATALOG_QUERY_CACHE_KEY_BYTES + 1), page);
    assert!(
        cache.entries.is_empty(),
        "oversized search keys are not cached"
    );
}

#[test]
fn db_catalog_generation_gap_never_mixes_web_browse_or_search_pages() {
    let app = testdata_app();
    let old_generation = app.update_id.load(Ordering::Acquire);
    let new_generation = rusty_dlna_protocol::soap::next_system_update_id(old_generation);
    let detail_id = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.title.contains("Fixture Movie"))
        .unwrap()
        .detail_id;
    let new_title = "A Generation Race Title";
    app.db_pool
        .as_ref()
        .unwrap()
        .write(|db| {
            let transaction = db.transaction()?;
            db.update_detail_title(detail_id, new_title)?;
            db.set_update_id(new_generation)?;
            transaction.commit()
        })
        .unwrap();

    let db_path = app.scan_cfg.db_path.as_deref().unwrap();
    let web_query_count = Arc::new(AtomicUsize::new(0));
    count_web_media_queries_for_test(db_path, Arc::clone(&web_query_count));
    let web = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&sort=title",
        "GenerationRace/1.0",
    )));
    stop_counting_web_media_queries_for_test(db_path);
    assert_eq!(
        web.status, 409,
        "web lists must reject an unstable snapshot"
    );
    assert_eq!(
        web_query_count.load(Ordering::Relaxed),
        3,
        "the web library owns one three-attempt retry loop"
    );

    let (status, browse) = soap_browse(
        &app,
        rusty_dlna_protocol::object_id::VIDEO_ALL_ID,
        "BrowseDirectChildren",
        "GenerationRace/1.0",
    );
    assert_eq!(status, 200, "{browse}");
    assert!(browse.contains("Fixture Movie"), "{browse}");
    assert!(!browse.contains(new_title), "{browse}");
    assert!(
        browse.contains(&format!("<UpdateID>{old_generation}</UpdateID>")),
        "{browse}"
    );

    let (status, search) = soap_action(
        &app,
        "Search",
        &format!(
            r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains &quot;{new_title}&quot;</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#
        ),
        "GenerationRace/1.0",
    );
    assert_eq!(status, 200, "{search}");
    assert!(!search.contains(new_title), "{search}");
    assert!(
        app.catalog_query_cache.lock().unwrap().entries.is_empty(),
        "an unstable DB page must not poison the old-generation cache"
    );

    {
        let mut catalog = write_recover(&app.catalog);
        for item in catalog.items.values_mut() {
            if item.detail_id == detail_id {
                item.title = new_title.into();
            }
        }
        app.update_id.store(new_generation, Ordering::Release);
    }
    let (status, browse) = soap_browse(
        &app,
        rusty_dlna_protocol::object_id::VIDEO_ALL_ID,
        "BrowseDirectChildren",
        "GenerationRace/1.0",
    );
    assert_eq!(status, 200, "{browse}");
    assert!(browse.contains(new_title), "{browse}");
    assert!(
        browse.contains(&format!("<UpdateID>{new_generation}</UpdateID>")),
        "{browse}"
    );
    let (status, search) = soap_action(
        &app,
        "Search",
        &format!(
            r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains &quot;{new_title}&quot;</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#
        ),
        "GenerationRace/1.0",
    );
    assert_eq!(status, 200, "{search}");
    assert!(search.contains(new_title), "{search}");
}

#[test]
fn web_item_samples_item_and_generation_under_one_catalog_snapshot() {
    let app = Arc::new(testdata_app());
    let old_generation = app.update_id.load(Ordering::Acquire);
    let (detail_id, old_title) = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.title == "Fixture Movie")
        .map(|item| (item.detail_id, item.title.clone()))
        .unwrap();
    let new_title = "Published After Item Snapshot";
    let mut replacement = read_recover(&app.catalog).clone();
    for item in replacement.items.values_mut() {
        if item.detail_id == detail_id {
            item.title = new_title.into();
        }
    }
    let reached = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    web_ui::pause_item_snapshot_for_test(
        &app,
        detail_id,
        Arc::clone(&reached),
        Arc::clone(&release),
    );

    let requester_app = Arc::clone(&app);
    let requester = std::thread::spawn(move || {
        requester_app.handle(&req(&get(
            &format!("/api/web/item/{detail_id}"),
            "ItemGenerationRace/1.0",
        )))
    });
    reached.wait();
    assert!(matches!(
        app.catalog.try_write(),
        Err(std::sync::TryLockError::WouldBlock)
    ));

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let publisher_app = Arc::clone(&app);
    let publisher = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = apply_catalog(
            &publisher_app,
            replacement,
            ScanDelta {
                changed: 1,
                ..ScanDelta::default()
            },
            "web item generation race",
        );
        done_tx.send(result).unwrap();
    });
    started_rx.recv().unwrap();
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "publication must wait for the item snapshot's catalog read guard"
    );
    release.wait();

    let response = requester.join().unwrap();
    assert_eq!(response.status, 200);
    let json: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(json["item"]["title"], old_title);
    assert_eq!(
        resp_header(&response, "ETag"),
        Some(format!("W/\"web-v2-r5-{old_generation}-item-{detail_id}\"").as_str())
    );
    done_rx.recv().unwrap().unwrap();
    publisher.join().unwrap();
    assert_eq!(
        app.update_id.load(Ordering::Acquire),
        rusty_dlna_protocol::soap::next_system_update_id(old_generation)
    );
    assert_eq!(
        read_recover(&app.catalog)
            .get_item_by_detail(detail_id)
            .unwrap()
            .title,
        new_title
    );
}

#[test]
fn system_update_id_wrap_clears_generation_zero_cache_and_labels_the_response() {
    let app = testdata_app();
    let stale_page = rusty_dlna_scan::CatalogQueryPage {
        object_ids: vec!["ancient-generation-zero".into()],
        total: 1,
        population: 1,
    };
    app.catalog_query_cache
        .lock()
        .unwrap()
        .insert(0, "ancient".into(), stale_page);
    app.update_id.store(u32::MAX, Ordering::Release);
    app.db_pool
        .as_ref()
        .unwrap()
        .write(|db| db.set_update_id(u32::MAX))
        .unwrap();
    let mut next = read_recover(&app.catalog).clone();
    let item = next.items.values_mut().next().unwrap();
    item.title = "Wrapped Generation Title".into();
    apply_catalog(
        &app,
        next,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "ui4 wrap test",
    )
    .unwrap();
    assert_eq!(app.update_id.load(Ordering::Acquire), 0);
    assert_eq!(
        app.db_pool
            .as_ref()
            .unwrap()
            .read(LibraryDb::get_update_id)
            .unwrap(),
        0
    );
    assert!(app.catalog_query_cache.lock().unwrap().entries.is_empty());
    let response = app.handle(&req(&get(
        "/api/web/library?view=library&generation=0",
        "GenerationWrap/1.0",
    )));
    assert_eq!(response.status, 200);
    let json: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(json["generation"], 0);
}

#[test]
fn cache_and_database_directories_are_independent_and_config_relative() {
    let test_tree = TestTree::new("split-dirs");
    let tmp = test_tree.path();
    let app = App::from_config(
        Config {
            cache_dir: Some("derived-cache".into()),
            db_dir: Some("database".into()),
            rescan_secs: 0,
            ..Config::default()
        },
        18200,
        11900,
        tmp,
    );
    assert_eq!(app.cache_dir, tmp.join("derived-cache"));
    assert_eq!(
        app.scan_cfg.db_path.as_deref(),
        Some(tmp.join("database/files.db").as_path())
    );
    assert!(tmp.join("database/files.db").is_file());
}

#[test]
fn startup_prunes_stale_derived_images_before_enforcing_shared_free_space() {
    let test_tree = TestTree::new("startup-derived-cache");
    let cache = test_tree.path().join("cache");
    let derived = cache.join("derived-images");
    std::fs::create_dir_all(&derived).unwrap();
    let stale = derived.join(format!("{}.jpg", "a".repeat(64)));
    std::fs::write(&stale, vec![0u8; 1024]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();

    let app = App::from_config(
        Config {
            cache_dir: Some(cache.display().to_string()),
            db_dir: Some(test_tree.path().join("db").display().to_string()),
            cache_min_free_mb: 0,
            rescan_secs: 0,
            ..Config::default()
        },
        18200,
        11900,
        test_tree.path(),
    );

    assert_eq!(app.cache_dir, cache);
    assert!(!stale.exists());
}

#[test]
fn invalid_disabled_transcode_cache_policy_stops_before_startup_maintenance() {
    let test_tree = TestTree::new("invalid-disabled-transcode-cache");
    let cache = test_tree.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let output = rusty_dlna_transcode::cache_dest_for_key(
        &cache,
        1,
        rusty_dlna_transcode::RecodeAction::Browser,
        &"a".repeat(64),
    );
    let abandoned_part = rusty_dlna_transcode::cache_part(&output);
    std::fs::write(&abandoned_part, b"must remain untouched").unwrap();

    let mut cfg = Config {
        cache_dir: Some(cache.display().to_string()),
        db_dir: Some(test_tree.path().join("db").display().to_string()),
        rescan_secs: 0,
        ..Config::default()
    };
    assert!(!cfg.transcode.enable);
    cfg.transcode.cache_max_mb = 0;

    let error = App::try_from_config(cfg, 18200, 11900, test_tree.path())
        .err()
        .expect("invalid cache maintenance policy");
    assert!(
        error.to_string().contains("transcode.cache_max_mb"),
        "{error}"
    );
    assert!(
        abandoned_part.is_file(),
        "validation must happen before startup cache cleanup"
    );
}

fn preflight_failure_leaves_storage_untouched(
    label: &str,
    mut cfg: Config,
    initialize: impl FnOnce(Config, &Path) -> Result<App, AppInitError>,
) -> AppInitError {
    let test_tree = TestTree::new(label);
    let cache = test_tree.path().join("cache");
    let database = test_tree.path().join("database");
    std::fs::create_dir_all(&cache).unwrap();
    let output = rusty_dlna_transcode::cache_dest_for_key(
        &cache,
        1,
        rusty_dlna_transcode::RecodeAction::Browser,
        &"b".repeat(64),
    );
    let stale_part = rusty_dlna_transcode::cache_part(&output);
    std::fs::write(&stale_part, b"preflight must not maintain this artifact").unwrap();
    cfg.cache_dir = Some(cache.display().to_string());
    cfg.db_dir = Some(database.display().to_string());
    cfg.rescan_secs = 0;

    let error = match initialize(cfg, test_tree.path()) {
        Ok(_) => panic!("invalid startup preflight unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(stale_part.is_file(), "{label}: cache artifact was changed");
    assert!(
        !database.join("files.db").exists(),
        "{label}: database was created"
    );
    assert!(!cache.join("uuid").exists(), "{label}: UUID was created");
    error
}

#[test]
fn ai_upscale_preflight_confines_and_hashes_the_configured_shader() {
    use sha2::Digest as _;
    use std::io::{Read as _, Seek as _};

    let test_tree = TestTree::new("ai-upscale-shader-preflight");
    let model_dir = test_tree.path().join("models");
    std::fs::create_dir_all(&model_dir).unwrap();
    let shader = b"//!HOOK LUMA\n//!BIND HOOKED\nvec4 hook(){return HOOKED_tex(HOOKED_pos);}\n";
    std::fs::write(model_dir.join("quality.glsl"), shader).unwrap();
    let cfg = Config {
        transcode: TranscodeCfg {
            enable: true,
            ..TranscodeCfg::default()
        },
        web: WebCfg {
            encoder: "h264_nvenc".into(),
            ai_upscale: vec![WebAiUpscaleCfg {
                name: "quality".into(),
                shader_path: "models/quality.glsl".into(),
                max_source_width: 1920,
                max_source_height: 1080,
                max_source_pixels_per_second: 52_000_000,
            }],
            ..WebCfg::default()
        },
        ..Config::default()
    };

    let preflight = App::preflight_config(cfg, 18200, 11900, test_tree.path()).unwrap();
    assert_eq!(preflight.ai_upscale_profiles.len(), 1);
    assert_eq!(preflight.ai_upscale_profiles[0].name, "quality");
    assert_eq!(
        preflight.ai_upscale_profiles[0].shader_sha256,
        sha2::Sha256::digest(shader)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    std::fs::write(model_dir.join("quality.glsl"), b"replaced after preflight").unwrap();
    let mut frozen = preflight.ai_upscale_profiles[0].shader_file.as_ref();
    frozen.rewind().unwrap();
    let mut retained = Vec::new();
    frozen.read_to_end(&mut retained).unwrap();
    frozen.rewind().unwrap();
    assert_eq!(retained, shader);

    let missing = preflight_failure_leaves_storage_untouched(
        "missing-ai-upscale-shader",
        Config {
            transcode: TranscodeCfg {
                enable: true,
                ..TranscodeCfg::default()
            },
            web: WebCfg {
                encoder: "h264_nvenc".into(),
                ai_upscale: vec![WebAiUpscaleCfg {
                    name: "missing".into(),
                    shader_path: "models/missing.glsl".into(),
                    max_source_width: 1920,
                    max_source_height: 1080,
                    max_source_pixels_per_second: 52_000_000,
                }],
                ..WebCfg::default()
            },
            ..Config::default()
        },
        |cfg, base| App::try_from_config(cfg, 18200, 11900, base),
    );
    assert!(matches!(missing, AppInitError::AiUpscaleShader { .. }));
}

#[test]
fn invalid_identity_and_network_preflight_are_storage_neutral() {
    let combined_invalid = preflight_failure_leaves_storage_untouched(
        "preflight-media-root-precedence",
        Config {
            media_dir: vec!["V,missing-media-root".into()],
            uuid: Some("broken".into()),
            listen_ip: Some("not-an-ipv4-address".into()),
            network_interface: vec!["definitely-missing0".into()],
            ..Config::default()
        },
        |cfg, base| {
            App::try_from_config_with_network(
                cfg,
                18200,
                11900,
                base,
                &[InterfaceV4 {
                    name: "loopback-only".into(),
                    addr: Ipv4Addr::LOCALHOST,
                    netmask: Ipv4Addr::new(255, 0, 0, 0),
                }],
                None,
            )
        },
    );
    assert!(matches!(combined_invalid, AppInitError::MediaRoots { .. }));

    let invalid_uuid = preflight_failure_leaves_storage_untouched(
        "preflight-invalid-uuid",
        Config {
            uuid: Some("broken".into()),
            ..Config::default()
        },
        |cfg, base| App::try_from_config(cfg, 18200, 11900, base),
    );
    assert!(matches!(invalid_uuid, AppInitError::Identity { .. }));

    let invalid_listen = preflight_failure_leaves_storage_untouched(
        "preflight-invalid-listen",
        Config {
            listen_ip: Some("not-an-ipv4-address".into()),
            ..Config::default()
        },
        |cfg, base| App::try_from_config(cfg, 18200, 11900, base),
    );
    assert!(matches!(invalid_listen, AppInitError::ListenAddress { .. }));

    let missing_interface = preflight_failure_leaves_storage_untouched(
        "preflight-missing-interface",
        Config {
            network_interface: vec!["definitely-missing0".into()],
            ..Config::default()
        },
        |cfg, base| {
            App::try_from_config_with_network(
                cfg,
                18200,
                11900,
                base,
                &[InterfaceV4 {
                    name: "loopback-only".into(),
                    addr: Ipv4Addr::LOCALHOST,
                    netmask: Ipv4Addr::new(255, 0, 0, 0),
                }],
                None,
            )
        },
    );
    assert!(matches!(
        missing_interface,
        AppInitError::Advertisement { .. }
    ));
}

#[test]
fn transcode_validation_is_actionable_and_disabled_mode_needs_no_tools() {
    let mut cfg = Config::default();
    cfg.transcode.enable = true;
    cfg.transcode.encoder = "copy".into();
    let error = match App::try_from_config(cfg, 18200, 11900, &workspace()) {
        Ok(_) => panic!("invalid transcode encoder was accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("must name a video encoder"),
        "{error}"
    );

    assert!(validate_transcode_tools(false, "not installed", &[])
        .unwrap()
        .is_empty());
    let missing_encoder = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
hdr = "dv-p7"
action = "hdr10"
encoder = "rusty_encoder_that_does_not_exist"
"#,
    )
    .unwrap();
    let error = validate_transcode_tools(true, "libx264", &missing_encoder).unwrap_err();
    assert!(error.to_string().contains("ffmpeg -encoders"), "{error}");
}

#[test]
fn runtime_keeps_validated_remaps_in_effective_config() {
    let test_tree = TestTree::new("effective-config-remaps");
    let remaps = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
name = "preserved-runtime-remap"
client = "Kodi"
action = "original"
"#,
    )
    .unwrap();
    let cfg = Config {
        cache_dir: Some(test_tree.path().join("cache").display().to_string()),
        db_dir: Some(test_tree.path().join("database").display().to_string()),
        remap: remaps.clone(),
        rescan_secs: 0,
        ..Config::default()
    };
    let app = App::try_from_config(cfg, 18200, 11900, test_tree.path()).unwrap();
    assert_eq!(app.cfg.remap, remaps);
    assert_eq!(app.remaps, remaps);
}

#[test]
fn uuid_is_validated_normalized_unique_and_persisted() {
    assert_eq!(
        normalize_uuid("4D696E69-444C-164E-9D41-98B7852028D3").unwrap(),
        "uuid:4d696e69-444c-164e-9d41-98b7852028d3"
    );
    assert_eq!(
        normalize_uuid("uuid:4d696e69-444c-164e-9d41-98b7852028d3").unwrap(),
        "uuid:4d696e69-444c-164e-9d41-98b7852028d3"
    );
    assert!(normalize_uuid("uuid:not-a-uuid").is_err());

    let test_tree = TestTree::new("uuid");
    let base = test_tree.path().to_path_buf();
    let first_dir = base.join("one");
    let second_dir = base.join("two");
    let first = load_or_create_uuid(&first_dir, None).unwrap();
    let again = load_or_create_uuid(&first_dir, None).unwrap();
    let second = load_or_create_uuid(&second_dir, None).unwrap();
    assert_eq!(first, again, "UUID must survive restart");
    assert_ne!(first, second, "independent caches need independent UUIDs");
    assert_eq!(&first[19..20], "4", "generated UUID must be version 4");
    assert!(matches!(&first[24..25], "8" | "9" | "a" | "b"));
    assert_eq!(
        std::fs::read_to_string(first_dir.join("uuid"))
            .unwrap()
            .trim(),
        first
    );

    let invalid = Config {
        uuid: Some("broken".into()),
        cache_dir: Some(base.join("invalid").display().to_string()),
        rescan_secs: 0,
        ..Config::default()
    };
    let error = App::try_from_config(invalid, 18200, 11900, &base)
        .err()
        .expect("invalid configured UUID");
    assert!(error.to_string().contains("uuid must be"), "{error}");
}

#[test]
fn advertisement_selection_validates_interfaces_and_live_addresses() {
    let interfaces = vec![
        InterfaceV4 {
            name: "eth0".into(),
            addr: "192.0.2.20".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
        },
        InterfaceV4 {
            name: "eth1".into(),
            addr: "198.51.100.8".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
        },
        InterfaceV4 {
            name: "lo".into(),
            addr: Ipv4Addr::LOCALHOST,
            netmask: "255.0.0.0".parse().unwrap(),
        },
    ];
    let live = rusty_dlna_protocol::ssdp::SSDP_PORT;
    assert_eq!(
        select_advertise_ip(
            Some("192.0.2.20"),
            Ipv4Addr::UNSPECIFIED,
            &[],
            live,
            &interfaces,
            None,
        )
        .unwrap(),
        "192.0.2.20".parse::<Ipv4Addr>().unwrap()
    );
    assert!(select_advertise_ip(
        Some("203.0.113.9"),
        Ipv4Addr::UNSPECIFIED,
        &[],
        live,
        &interfaces,
        None,
    )
    .unwrap_err()
    .contains("not assigned"));
    assert!(select_advertise_ip(
        Some("127.0.0.1"),
        Ipv4Addr::UNSPECIFIED,
        &[],
        live,
        &interfaces,
        None,
    )
    .unwrap_err()
    .contains("loopback"));
    assert_eq!(
        select_advertise_ip(
            Some("127.0.0.1"),
            Ipv4Addr::UNSPECIFIED,
            &[],
            11900,
            &interfaces,
            None,
        )
        .unwrap(),
        Ipv4Addr::LOCALHOST
    );
    assert_eq!(
        select_advertise_ip(
            None,
            Ipv4Addr::UNSPECIFIED,
            &["eth1".into()],
            live,
            &interfaces,
            None,
        )
        .unwrap(),
        "198.51.100.8".parse::<Ipv4Addr>().unwrap()
    );
    assert!(select_advertise_ip(
        None,
        Ipv4Addr::UNSPECIFIED,
        &["missing0".into()],
        live,
        &interfaces,
        None,
    )
    .unwrap_err()
    .contains("no enabled IPv4"));
    assert_eq!(
        select_advertise_ip(
            None,
            Ipv4Addr::UNSPECIFIED,
            &[],
            live,
            &interfaces,
            Some("eth0"),
        )
        .unwrap(),
        "192.0.2.20".parse::<Ipv4Addr>().unwrap()
    );
    assert!(
        select_advertise_ip(None, Ipv4Addr::UNSPECIFIED, &[], live, &interfaces, None,)
            .unwrap_err()
            .contains("multiple LAN")
    );
    assert!(
        select_advertise_ip(None, Ipv4Addr::UNSPECIFIED, &[], live, &[], None,)
            .unwrap_err()
            .contains("no usable LAN")
    );

    let selected = select_ssdp_interfaces(
        &["eth0".into(), "eth1".into()],
        "192.0.2.20".parse().unwrap(),
        live,
        &interfaces,
    )
    .unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(
        reply_interface_for_sender(
            "198.51.100.77".parse().unwrap(),
            &selected,
            "192.0.2.20".parse().unwrap(),
        ),
        "198.51.100.8".parse::<Ipv4Addr>().unwrap()
    );
    let packets = msearch_replies(
        "uuid:test",
        "upnp:rootdevice",
        "198.51.100.8",
        8200,
        900,
        "Linux/1 UPnP/1.0 rustyDLNA/1",
        "Tue, 18 Aug 2026 00:00:00 GMT",
    );
    assert!(packets[0].contains("LOCATION: http://198.51.100.8:8200/rootDesc.xml"));

    let mut same_name = interfaces.clone();
    same_name.push(InterfaceV4 {
        name: "eth0".into(),
        addr: "192.0.2.21".parse().unwrap(),
        netmask: "255.255.255.0".parse().unwrap(),
    });
    let selected = select_ssdp_interfaces(
        &["eth0".into()],
        "192.0.2.20".parse().unwrap(),
        live,
        &same_name,
    )
    .unwrap();
    assert_eq!(selected.len(), 2, "all IPv4 addresses on a named interface");
    assert_eq!(
        reply_interface_for_sender(
            "192.0.2.77".parse().unwrap(),
            &selected,
            "192.0.2.20".parse().unwrap(),
        ),
        "192.0.2.20".parse::<Ipv4Addr>().unwrap(),
        "equal-prefix aliases must prefer the configured primary"
    );
}

#[test]
fn renderer_location_policy_blocks_spoofed_and_off_link_targets() {
    let interfaces = vec![InterfaceV4 {
        name: "eth0".into(),
        addr: "192.0.2.20".parse().unwrap(),
        netmask: "255.255.255.0".parse().unwrap(),
    }];
    let sender: SocketAddr = "192.0.2.55:1900".parse().unwrap();
    assert_eq!(
        trusted_renderer_location(
            "http://192.0.2.55:1400/description.xml",
            sender,
            &interfaces,
        ),
        Some((
            "192.0.2.55".parse().unwrap(),
            1400,
            "/description.xml".into()
        ))
    );
    for url in [
        "http://192.0.2.99/description.xml",
        "http://127.0.0.1/description.xml",
        "http://239.255.255.250/description.xml",
        "http://192.0.2.55@127.0.0.1/description.xml",
        "http://192.0.2.55/description.xml\r\nX-Evil: yes",
    ] {
        assert!(
            trusted_renderer_location(url, sender, &interfaces).is_none(),
            "trusted spoofed URL {url:?}"
        );
    }
    assert!(trusted_renderer_location(
        "http://198.51.100.55/description.xml",
        "198.51.100.55:1900".parse().unwrap(),
        &interfaces,
    )
    .is_none());
}

#[test]
fn renderer_http_response_requires_bounded_successful_xml() {
    let body = b"<root><friendlyName>Living Room</friendlyName></root>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut valid = response.into_bytes();
    valid.extend_from_slice(body);
    assert_eq!(renderer_xml_body(&valid), Some(body.as_slice()));

    for response in [
        b"HTTP/1.1 404 Not Found\r\nContent-Type: text/xml\r\nContent-Length: 0\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: 9999999\r\n\r\n",
    ] {
        assert!(renderer_xml_body(response).is_none());
    }
    let mut oversized = b"HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\n\r\n".to_vec();
    oversized.resize(oversized.len() + MAX_RENDERER_DESCRIPTION_BYTES + 1, b'x');
    assert!(renderer_xml_body(&oversized).is_none());
}

#[test]
fn renderer_and_msearch_limiters_deduplicate_floods() {
    let sender: Ipv4Addr = "192.0.2.55".parse().unwrap();
    let now = std::time::Instant::now();
    let mut renderer = RendererFetchLimiter::default();
    assert!(renderer.allow(sender, "one", now));
    assert!(!renderer.allow(sender, "one", now));
    assert!(renderer.allow(sender, "two", now));
    assert!(renderer.allow(sender, "three", now));
    assert!(renderer.allow(sender, "four", now));
    assert!(!renderer.allow(sender, "five", now));
    assert!(renderer.allow(sender, "five", now + Duration::from_secs(61)));

    let mut replies = SsdpReplyLimiter::default();
    assert!(replies.allow(sender, 6, now));
    assert!(replies.allow(sender, 6, now));
    assert!(!replies.allow(sender, 1, now));
    assert!(replies.allow(sender, 6, now + Duration::from_secs(2)));
}

#[test]
fn renderer_fetch_has_a_total_slow_response_deadline() {
    use std::io::Read;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request);
        std::thread::sleep(Duration::from_millis(1500));
    });
    let started = std::time::Instant::now();
    assert!(fetch_renderer_description(Ipv4Addr::LOCALHOST, port, "/slow").is_none());
    assert!(started.elapsed() < Duration::from_millis(1400));
    server.join().unwrap();
}

#[test]
fn rootdesc_mediaserver_and_xbox() {
    let app = testdata_app();
    let r = app.handle(&req(&get("/rootDesc.xml", "Kodi/21.0")));
    let body = String::from_utf8_lossy(&r.body);
    assert_eq!(r.status, 200);
    assert!(body.contains("MediaServer:1"));
    assert!(body.contains("/ctl/ContentDir"));
    assert!(body.contains("/icons/sm.png"));
    let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/1.0")));
    let xb = String::from_utf8_lossy(&xbox.body);
    assert!(xb.contains("<modelNumber>1</modelNumber>"));
    assert!(xb.contains(": 1"));
    let tv = app.handle(&req(&get(
        "/rootDesc.xml",
        "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0",
    )));
    let t = String::from_utf8_lossy(&tv.body);
    assert!(t.contains("sec:ProductCap"));
}

#[test]
fn kodi_scpd_and_subscribe() {
    let app = testdata_app();
    let scpd = app.handle(&req(&get("/ContentDir.xml", "Kodi/21.0 Platinum/1.0.5.13")));
    assert_eq!(scpd.status, 200);
    let body = String::from_utf8_lossy(&scpd.body);
    assert!(body.contains("<name>Browse</name>"), "{body}");
    assert!(body.contains("BrowseDirectChildren"), "{body}");
    let sub = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:8200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
    let peer: SocketAddr = "192.0.2.50:1234".parse().unwrap();
    let r = app.handle_from(&sub, peer);
    assert_eq!(r.status, 200, "Platinum SUBSCRIBE must not 404");
    let sid = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("SID"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(sid.starts_with("uuid:"), "{sid}");
}

fn accept_notify(listener: &std::net::TcpListener, timeout: std::time::Duration) -> String {
    use std::io::{Read, Write};
    let start = std::time::Instant::now();
    listener.set_nonblocking(true).ok();
    loop {
        match listener.accept() {
            Ok((mut sock, _)) => {
                listener.set_nonblocking(false).ok();
                sock.set_nonblocking(false).ok();
                sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .ok();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    match sock.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buf.extend_from_slice(&chunk[..read]);
                            let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                            else {
                                continue;
                            };
                            let headers = String::from_utf8_lossy(&buf[..header_end]);
                            let content_length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("Content-Length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if buf.len() >= header_end + 4 + content_length {
                                break;
                            }
                        }
                    }
                }
                let _ = sock.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return String::from_utf8_lossy(&buf).into_owned();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() > timeout {
                    panic!("timed out waiting for GENA NOTIFY");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("notify accept: {e}"),
        }
    }
}

#[test]
fn gena_subscribe_rules() {
    let app = testdata_app();
    let peer50: SocketAddr = "192.0.2.50:1234".parse().unwrap();
    let peer1: SocketAddr = "192.0.2.1:9".parse().unwrap();
    let new_ok = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
    let r = app.handle_from(&new_ok, peer50);
    assert_eq!(r.status, 200);
    let sid = resp_header(&r, "SID").unwrap_or("");
    assert!(sid.starts_with("uuid:"), "{sid}");
    assert_eq!(
        uuid::Uuid::parse_str(sid.trim_start_matches("uuid:"))
            .unwrap()
            .get_version_num(),
        4
    );
    assert_eq!(resp_header(&r, "Timeout"), Some("Second-300"));

    let inject = "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt\nX-Injected: 1>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n";
    assert!(matches!(
        HttpRequest::parse_headers(inject),
        Err(rusty_dlna_http::ParseError::InvalidHeaderValue)
    ));

    let mismatch = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
    assert_eq!(app.handle_from(&mismatch, peer1).status, 412);

    let both = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             SID: uuid:already-have-one\r\n\
             NT: upnp:event\r\n\
             Content-Length: 0\r\n\r\n");
    assert_eq!(app.handle_from(&both, peer50).status, 400);

    let no_nt = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
    assert_eq!(app.handle_from(&no_nt, peer50).status, 400);

    let renew_unknown = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
    assert_eq!(app.handle_from(&renew_unknown, peer50).status, 412);

    let unsub_unknown = req("UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Content-Length: 0\r\n\r\n");
    assert_eq!(app.handle_from(&unsub_unknown, peer50).status, 412);

    let renew = req(&format!(
        "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: {sid}\r\n\
             Timeout: Second-1800\r\n\
             Content-Length: 0\r\n\r\n"
    ));
    let r = app.handle_from(&renew, peer50);
    assert_eq!(r.status, 200);
    assert_eq!(resp_header(&r, "Timeout"), Some("Second-1800"));
    assert_eq!(resp_header(&r, "SID"), Some(sid));

    let renew_low = req(&format!(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: 192.0.2.10:18200\r\nSID: {sid}\r\nTimeout: Second-1\r\nContent-Length: 0\r\n\r\n"
        ));
    let r = app.handle_from(&renew_low, peer50);
    assert_eq!(resp_header(&r, "Timeout"), Some("Second-30"));
}

#[test]
fn gena_notify_on_catalog_bump() {
    let app = testdata_app();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("notify listener");
    let addr = listener.local_addr().expect("listener addr");
    let port = addr.port();
    assert_ne!(port, 8200, "test callback must not be LAN :8200");
    let sub = req(&format!(
        "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 127.0.0.1:18200\r\n\
             Callback: <http://127.0.0.1:{port}/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n"
    ));
    let r = app.handle_from(&sub, addr);
    assert_eq!(r.status, 200);
    let n0 = accept_notify(&listener, std::time::Duration::from_secs(3));
    assert!(
        n0.contains("NTS: upnp:propchange") || n0.contains("NTS:upnp:propchange"),
        "{n0}"
    );
    assert!(n0.contains("SystemUpdateID"), "{n0}");
    assert!(n0.contains("SEQ: 0"), "{n0}");
    let id0 = n0
        .split("<SystemUpdateID>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .unwrap_or("");
    let before = app.update_id.load(Ordering::Relaxed);
    let cat = read_recover(&app.catalog).clone();
    apply_catalog(
        &app,
        cat,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "test catalog bump",
    )
    .unwrap();
    let after = app.update_id.load(Ordering::Relaxed);
    assert!(after > before, "update_id {before} -> {after}");
    let n1 = accept_notify(&listener, std::time::Duration::from_secs(3));
    assert!(
        n1.contains("NTS: upnp:propchange") || n1.contains("NTS:upnp:propchange"),
        "{n1}"
    );
    assert!(n1.contains("SEQ: 1"), "{n1}");
    assert!(
        n1.contains(&format!("<SystemUpdateID>{after}</SystemUpdateID>")),
        "{n1}"
    );
    if !id0.is_empty() {
        assert_ne!(id0, after.to_string());
    }
    if let Some(p) = app.scan_cfg.db_path.as_ref() {
        let db = LibraryDb::open(p).expect("db");
        assert_eq!(db.get_update_id().unwrap(), after);
    }
}

#[test]
fn catalog_publication_failure_keeps_catalog_and_update_id_in_sync() {
    let app = testdata_app();
    let path = app.scan_cfg.db_path.as_ref().unwrap().clone();
    let mut next = app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let item_id = next.items.keys().next().expect("fixture item").clone();
    let old_title = next.items[&item_id].title.clone();
    next.items.get_mut(&item_id).unwrap().title = "must-not-publish".into();
    let old_update_id = app.update_id.load(Ordering::Relaxed);

    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let result = apply_catalog(
        &app,
        next,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "injected publication failure",
    );
    assert!(result.is_err());
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(app.update_id.load(Ordering::Relaxed), old_update_id);
    assert_eq!(
        app.catalog
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .items[&item_id]
            .title,
        old_title
    );
}

#[test]
fn failed_staged_publication_reopens_writer_and_rebacks_up_for_retry() {
    let app = testdata_app();
    let before = app.update_id.load(Ordering::Acquire);
    let prepared = prepare_scan_change(&app, ScanSession::prepare_rebuild_objects).unwrap();
    let pool = app.db_pool.as_ref().unwrap();
    pool.write(|db| {
        let epoch = db
            .setting("scan_catalog_epoch")?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        db.set_setting("scan_catalog_epoch", &(epoch + 1).to_string())
    })
    .unwrap();

    let error = apply_prepared_catalog_change(&app, prepared, "injected stale staged publication")
        .unwrap_err();
    assert!(error.to_string().contains("epoch"), "{error}");
    assert_eq!(app.update_id.load(Ordering::Acquire), before);
    assert_eq!(
        pool.write(LibraryDb::get_update_id).unwrap(),
        before,
        "the pooled writer must be usable immediately after the failed attach/merge path"
    );

    let retry = prepare_scan_change(&app, ScanSession::prepare_rebuild_objects).unwrap();
    apply_prepared_catalog_change(&app, retry, "staged publication retry").unwrap();
    let after = rusty_dlna_protocol::soap::next_system_update_id(before);
    assert_eq!(app.update_id.load(Ordering::Acquire), after);
    assert_eq!(pool.write(LibraryDb::get_update_id).unwrap(), after);
}

#[test]
fn catalog_publication_without_its_required_database_pool_fails_closed() {
    let mut app = testdata_app();
    let movie = movie_fixture(&app);
    let stale = read_recover(&app.catalog).clone();
    app.db_pool = None;

    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>180</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");
    let bookmark_generation = app.update_id.load(Ordering::Acquire);

    let error = apply_catalog(
        &app,
        stale,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "missing database pool regression",
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("requires the application database pool"));
    assert_eq!(app.update_id.load(Ordering::Acquire), bookmark_generation);
    assert_eq!(
        read_recover(&app.catalog)
            .get_item_by_detail(movie.detail_id)
            .unwrap()
            .bookmark_sec,
        180,
        "an impossible no-pool publication must preserve newer memory state"
    );
}

#[test]
fn catalog_hydration_failure_rolls_back_persisted_generation() {
    let app = testdata_app();
    let mut stale = read_recover(&app.catalog).clone();
    let item_id = stale.items.keys().next().unwrap().clone();
    let old_title = read_recover(&app.catalog).items[&item_id].title.clone();
    stale.items.get_mut(&item_id).unwrap().title = "must-not-publish".into();
    let old_generation = app.update_id.load(Ordering::Acquire);
    let path = app.scan_cfg.db_path.as_ref().unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute("DROP TABLE BOOKMARKS", [])
        .unwrap();

    let result = apply_catalog(
        &app,
        stale,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "hydration rollback regression",
    );
    assert!(result.is_err());
    assert_eq!(app.update_id.load(Ordering::Acquire), old_generation);
    assert_eq!(read_recover(&app.catalog).items[&item_id].title, old_title);
    let db = LibraryDb::open(path).unwrap();
    assert_eq!(db.get_update_id().unwrap(), old_generation);
}

#[test]
fn full_catalog_publication_rehydrates_bookmark_created_after_snapshot() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let stale = read_recover(&app.catalog).clone();
    assert_eq!(
        stale
            .get_item_by_detail(movie.detail_id)
            .unwrap()
            .bookmark_sec,
        0
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let callback = listener.local_addr().unwrap();
    let subscribe = req(&format!(
        "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
         Host: 127.0.0.1:18200\r\n\
         Callback: <http://127.0.0.1:{}/evt>\r\n\
         NT: upnp:event\r\n\
         Timeout: Second-300\r\n\
         Content-Length: 0\r\n\r\n",
        callback.port()
    ));
    assert_eq!(app.handle_from(&subscribe, callback).status, 200);
    let _initial = accept_notify(&listener, Duration::from_secs(3));

    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>180</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");
    let bookmark_generation = app.update_id.load(Ordering::Acquire);
    let bookmark_event = accept_notify(&listener, Duration::from_secs(3));
    assert!(
        bookmark_event.contains(&format!(
            "<SystemUpdateID>{bookmark_generation}</SystemUpdateID>"
        )),
        "{bookmark_event}"
    );

    apply_catalog(
        &app,
        stale,
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "stale full catalog regression",
    )
    .unwrap();

    let published_generation = app.update_id.load(Ordering::Acquire);
    assert_eq!(published_generation, bookmark_generation.saturating_add(1));
    let scan_event = accept_notify(&listener, Duration::from_secs(3));
    assert!(
        scan_event.contains(&format!(
            "<SystemUpdateID>{published_generation}</SystemUpdateID>"
        )),
        "{scan_event}"
    );
    assert!(app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .items
        .values()
        .filter(|item| item.detail_id == movie.detail_id)
        .all(|item| item.bookmark_sec == 180));
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((180, 0)));
    assert_eq!(db.get_update_id().unwrap(), published_generation);
    let (status, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(status, 200, "{xml}");
    assert!(xml.contains("lastPlaybackPosition&gt;180"), "{xml}");
}

#[test]
fn incremental_catalog_publication_rehydrates_state_created_after_patch() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let stale_title = "stale incremental patch title";
    let stale_patch = app
        .db_pool
        .as_ref()
        .unwrap()
        .write(|db| {
            db.begin_catalog_change_capture()?;
            db.update_detail_title(movie.detail_id, stale_title)?;
            db.load_catalog_patch()
        })
        .unwrap();
    assert!(format!("{stale_patch:?}").contains(stale_title));

    let new_tags = "&lt;upnp:playCount&gt;3&lt;/upnp:playCount&gt;&lt;upnp:lastPlaybackPosition&gt;90&lt;/upnp:lastPlaybackPosition&gt;";
    let (status, xml) = soap_action(
        &app,
        "UpdateObject",
        &format!(
            "<ObjectID>{}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");
    let bookmark_generation = app.update_id.load(Ordering::Acquire);

    apply_catalog_update(
        &app,
        CatalogUpdate::Patch(stale_patch),
        ScanDelta {
            changed: 1,
            ..ScanDelta::default()
        },
        "stale incremental catalog regression",
    )
    .unwrap();

    let published_generation = app.update_id.load(Ordering::Acquire);
    assert_eq!(published_generation, bookmark_generation.saturating_add(1));
    let published = app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        published.get_item_by_detail(movie.detail_id).unwrap().title,
        stale_title,
        "the nonempty stale patch must visibly apply its database change"
    );
    assert!(
        published
            .items
            .values()
            .filter(|item| item.detail_id == movie.detail_id)
            .all(|item| (item.bookmark_sec, item.watch_count) == (90, 3)),
        "every alias must use the database state hydrated at publication"
    );
    drop(published);
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((90, 3)));
    assert_eq!(db.get_update_id().unwrap(), published_generation);
}

#[test]
fn database_pool_restores_connections_and_activity_after_query_panics() {
    let app = testdata_app();
    let path = app.scan_cfg.db_path.as_ref().unwrap();
    let pool = Arc::new(DbPool::open(path, 1).unwrap());
    let before = pool.metrics();

    for _ in 0..2 {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.read::<()>(|_| panic!("injected reader panic"))
        }));
        assert!(panic.is_err());
        let metrics = pool.metrics();
        assert_eq!(metrics.read_active, 0);
        assert_eq!(metrics.readers_available, 1);
    }

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.write::<()>(|_| panic!("injected writer panic"))
    }));
    assert!(panic.is_err());
    assert!(!pool.metrics().writer_active);

    let cancellation = CancellationToken::default();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.write_scan::<(), rusqlite::Error>(&cancellation, |_| {
            panic!("injected scan writer panic")
        })
    }));
    assert!(panic.is_err());
    assert!(!pool.metrics().writer_active);

    let update_id = pool.read(LibraryDb::get_update_id).unwrap();
    assert!(update_id > 0);
    pool.write(|db| db.set_update_id(update_id)).unwrap();
    pool.write_scan(&cancellation, |db| db.set_update_id(update_id))
        .unwrap();

    let after = pool.metrics();
    assert_eq!(after.readers_available, after.reader_count);
    assert_eq!(after.read_active, 0);
    assert_eq!(after.read_waiters, 0);
    assert!(!after.writer_active);
    assert_eq!(after.reads_total, before.reads_total + 3);
    assert_eq!(after.writes_total, before.writes_total + 4);
    assert_eq!(after.errors_total, before.errors_total + 4);
}

#[test]
fn database_reader_panic_returns_connection_and_notifies_waiter() {
    let app = testdata_app();
    let path = app.scan_cfg.db_path.as_ref().unwrap();
    let pool = Arc::new(DbPool::open(path, 1).unwrap());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();

    let panicking_pool = Arc::clone(&pool);
    let panicking = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panicking_pool.read::<()>(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                panic!("injected reader panic with waiter")
            })
        }))
        .is_err()
    });
    entered_rx.recv().unwrap();

    let waiting_pool = Arc::clone(&pool);
    let waiting = std::thread::spawn(move || waiting_pool.read(LibraryDb::get_update_id));
    let deadline = Instant::now() + Duration::from_secs(2);
    while pool.metrics().read_waiters == 0 {
        assert!(Instant::now() < deadline, "reader did not enter wait state");
        std::thread::yield_now();
    }

    release_tx.send(()).unwrap();
    assert!(panicking.join().unwrap());
    assert!(waiting.join().unwrap().unwrap() > 0);
    let metrics = pool.metrics();
    assert_eq!(metrics.readers_available, 1);
    assert_eq!(metrics.read_active, 0);
    assert_eq!(metrics.read_waiters, 0);
}

#[test]
fn database_refresh_flag_is_cleared_when_refresh_unwinds() {
    let refreshing = Arc::new(AtomicBool::new(true));
    let thread_flag = Arc::clone(&refreshing);
    let refresh = std::thread::spawn(move || {
        let _refresh = RefreshInFlightGuard::new(thread_flag);
        panic!("injected integrity refresh panic");
    });
    assert!(refresh.join().is_err());
    assert!(!refreshing.load(Ordering::Acquire));
}

#[test]
fn host_and_timeseek_errors() {
    let app = testdata_app();
    let no_host = req("GET /rootDesc.xml HTTP/1.1\r\n\r\n");
    assert_eq!(app.handle(&no_host).status, 400);
    let local = req("GET /rootDesc.xml HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(app.handle(&local).status, 400);
    let ts = req(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nTimeSeekRange.dlna.org: npt=0-\r\n\r\n",
        );
    assert_eq!(app.handle(&ts).status, 406);

    let soap_rebind = req(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: attacker.example\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: 0\r\n\r\n",
        );
    assert_eq!(app.handle(&soap_rebind).status, 400);
    let soap_no_host = req(
            "POST /ctl/ContentDir HTTP/1.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: 0\r\n\r\n",
        );
    assert_eq!(app.handle(&soap_no_host).status, 400);
    let sub_rebind = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: localhost\r\nCallback: <http://127.0.0.1:9/e>\r\nNT: upnp:event\r\n\r\n",
        );
    assert_eq!(app.handle(&sub_rebind).status, 400);
    let unsub_rebind =
        req("UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: evil.test\r\nSID: uuid:x\r\n\r\n");
    assert_eq!(app.handle(&unsub_rebind).status, 400);
}

#[test]
fn original_media_read_errors_keep_diagnostics_out_of_the_response() {
    let response = crate::http_app::media_read_error_response(
        Path::new("/private/media/secret-title.mkv"),
        "Secret title",
        &std::io::Error::other("sentinel kernel diagnostic"),
    );
    let body = String::from_utf8(response.body).unwrap();

    assert_eq!(response.status, 500);
    assert!(body.contains("The media file could not be read."), "{body}");
    assert!(!body.contains("/private/media"), "{body}");
    assert!(!body.contains("Secret title"), "{body}");
    assert!(!body.contains("sentinel kernel diagnostic"), "{body}");
}

#[test]
fn sidecar_size_and_symlink_jail() {
    let app = testdata_app();
    let (art_id, detail_id, caption_index) = {
        let cat = read_recover(&app.catalog);
        let movie = cat
            .items
            .values()
            .find(|i| i.path.ends_with("movie.mkv"))
            .expect("movie");
        (
            movie.album_art,
            movie.detail_id,
            movie.captions.first().expect("caption fixture").index,
        )
    };
    assert!(art_id > 0);

    let outside_tree = TestTree::new("sidecar-outside");
    let outside = outside_tree.path().join(format!("secret-{detail_id}"));
    std::fs::write(&outside, b"not-a-poster").unwrap();
    {
        let mut cat = write_recover(&app.catalog);
        cat.album_art_paths.insert(art_id, outside.clone());
    }
    let escaped = app.handle(&req(&get(
        &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
        "Kodi/21.0",
    )));
    assert_eq!(escaped.status, 404, "path outside media/cache must 404");

    let cache = app.cache_dir.clone();
    let _ = std::fs::create_dir_all(&cache);
    let big = cache.join(format!(
        "rdlna-oversized-{}-{}.jpg",
        std::process::id(),
        detail_id
    ));
    {
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(rusty_dlna_scan::MAX_SIDECAR_BYTES + 1).unwrap();
    }
    {
        let mut cat = write_recover(&app.catalog);
        cat.album_art_paths.insert(art_id, big.clone());
    }
    let huge = app.handle(&req(&get(
        &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
        "Kodi/21.0",
    )));
    assert_eq!(huge.status, 413, "oversized sidecar must 413");

    let link = cache.join(format!(
        "rdlna-escape-{}-{}.jpg",
        std::process::id(),
        detail_id
    ));
    let _ = std::os::unix::fs::symlink(&outside, &link);
    {
        let mut cat = write_recover(&app.catalog);
        cat.album_art_paths.insert(art_id, link.clone());
    }
    let via_link = app.handle(&req(&get(
        &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
        "Kodi/21.0",
    )));
    assert_eq!(via_link.status, 404, "symlink out of tree must 404");

    let set_caption = |path: &Path, extension: &str| {
        let mut cat = write_recover(&app.catalog);
        let object_id = cat.by_detail[&detail_id].clone();
        let caption = cat
            .items
            .get_mut(&object_id)
            .unwrap()
            .captions
            .iter_mut()
            .find(|caption| caption.index == caption_index)
            .unwrap();
        caption.path = path.to_path_buf();
        caption.ext = extension.into();
    };
    let web_caption_url = format!("/Captions/{detail_id}/{caption_index}.srt?format=webvtt");
    set_caption(&outside, "srt");
    assert_eq!(
        app.handle(&req(&get(&web_caption_url, "Browser/1.0")))
            .status,
        404,
        "browser captions must retain the sidecar path jail"
    );
    set_caption(&big, "srt");
    assert_eq!(
        app.handle(&req(&get(&web_caption_url, "Browser/1.0")))
            .status,
        413,
        "browser captions must retain the sidecar size bound"
    );
    set_caption(&link, "srt");
    assert_eq!(
        app.handle(&req(&get(&web_caption_url, "Browser/1.0")))
            .status,
        404,
        "browser captions must reject a symlink retarget outside the roots"
    );
    let malformed = cache.join(format!(
        "rdlna-malformed-{}-{detail_id}.srt",
        std::process::id()
    ));
    std::fs::write(&malformed, b"not a subtitle cue").unwrap();
    set_caption(&malformed, "srt");
    let malformed_response = app.handle(&req(&get(&web_caption_url, "Browser/1.0")));
    assert_eq!(malformed_response.status, 422);
    let malformed_json: serde_json::Value =
        serde_json::from_slice(&malformed_response.body).unwrap();
    assert_eq!(malformed_json["error"]["code"], "caption_malformed");
    for unsupported in ["sub", "unknown"] {
        set_caption(&malformed, unsupported);
        let response = app.handle(&req(&get(&web_caption_url, "Browser/1.0")));
        assert_eq!(response.status, 415, "{unsupported}");
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["error"]["code"], "caption_unsupported");
    }
    let _ = std::fs::remove_file(malformed);

    let _ = std::fs::remove_file(&big);
    let _ = std::fs::remove_file(&link);
}

#[cfg(unix)]
#[test]
fn http_uses_the_same_wide_links_policy_as_the_scanner() {
    let mut app = testdata_app();
    let root = app.scan_cfg.media_dirs[0].join("video");
    let suffix = format!("{}", std::process::id());
    let outside_tree = TestTree::new("wide-source");
    let outside = outside_tree.path().join(format!("source-{suffix}.mkv"));
    let link = root.join(format!("wide-link-{suffix}.mkv"));
    std::fs::write(&outside, b"outside media bytes").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let mut item = movie_fixture(&app);
    item.detail_id = 9_100_001;
    item.object_id = "wide-link-object".into();
    item.path = link.clone();
    item.size = std::fs::metadata(&outside).unwrap().len();
    {
        let mut cat = app.catalog.write().unwrap();
        cat.by_detail.insert(item.detail_id, item.object_id.clone());
        cat.items.insert(item.object_id.clone(), item.clone());
    }
    let request = req(&format!(
        "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        item.detail_id
    ));
    assert_eq!(app.handle(&request).status, 403);

    app.cfg.wide_links = true;
    app.scan_cfg.wide_links = true;
    let allowed = app.handle(&request);
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.body, b"outside media bytes");

    let _ = std::fs::remove_file(&link);
}

#[cfg(unix)]
#[test]
fn large_media_response_streams_the_validated_descriptor_after_retarget() {
    use std::io::{Read, Seek};

    let mut app = testdata_app();
    let tree = TestTree::new("media-descriptor-race");
    let root = tree.path().join("media");
    let outside = tree.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let inside = root.join("inside.mkv");
    let secret = outside.join("secret.mkv");
    std::fs::write(&inside, b"allowed-media").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&inside)
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    std::fs::write(&secret, b"outside-secret").unwrap();
    let alias = root.join("movie.mkv");
    std::os::unix::fs::symlink(&inside, &alias).unwrap();
    app.scan_cfg.media_dirs = vec![root];

    let mut item = movie_fixture(&app);
    item.detail_id = 9_100_002;
    item.object_id = "descriptor-race-object".into();
    item.path = alias.clone();
    item.size = std::fs::metadata(&inside).unwrap().len();
    {
        let mut catalog = app.catalog.write().unwrap();
        catalog
            .by_detail
            .insert(item.detail_id, item.object_id.clone());
        catalog.items.insert(item.object_id.clone(), item.clone());
    }
    let request = req(&format!(
        "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        item.detail_id
    ));
    let response = app.handle(&request);
    assert_eq!(response.status, 200);
    let range = response.file_range.expect("large response descriptor");

    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    let mut opened = range.file.try_clone().unwrap();
    opened.seek(std::io::SeekFrom::Start(range.start)).unwrap();
    let mut prefix = [0u8; 13];
    opened.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"allowed-media");
    assert_eq!(app.handle(&request).status, 403);
}

#[cfg(unix)]
#[test]
fn sidecar_retarget_never_returns_outside_bytes() {
    let mut app = testdata_app();
    let tree = TestTree::new("sidecar-descriptor-race");
    let root = tree.path().join("media");
    let outside = tree.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let inside = root.join("inside.srt");
    let secret = outside.join("secret.srt");
    std::fs::write(&inside, b"allowed subtitle").unwrap();
    std::fs::write(&secret, b"outside secret subtitle").unwrap();
    let alias = root.join("movie.srt");
    std::os::unix::fs::symlink(&inside, &alias).unwrap();
    app.scan_cfg.media_dirs = vec![root.clone()];

    for index in 0..200 {
        let next = root.join(format!("sidecar-next-{index}"));
        let target = if index % 2 == 0 { &inside } else { &secret };
        std::os::unix::fs::symlink(target, &next).unwrap();
        std::fs::rename(&next, &alias).unwrap();
        if let Ok(body) = app.read_sidecar(&alias) {
            assert_eq!(body, b"allowed subtitle");
        }
    }
}

#[cfg(unix)]
#[test]
fn resized_request_keeps_the_authorized_source_descriptor_during_queue_wait() {
    let mut app = testdata_app();
    let tree = TestTree::new("resize-descriptor-race");
    let root = tree.path().join("media");
    let outside = tree.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let inside = root.join("inside.jpg");
    let secret = outside.join("secret.jpg");
    let existing_art = {
        let catalog = read_recover(&app.catalog);
        catalog
            .album_art_paths
            .values()
            .next()
            .cloned()
            .expect("fixture album art")
    };
    std::fs::copy(existing_art, &inside).unwrap();
    std::fs::write(&secret, b"outside secret is not an image").unwrap();
    let alias = root.join("queued.jpg");
    std::os::unix::fs::symlink(&inside, &alias).unwrap();
    app.scan_cfg.media_dirs = vec![root.clone()];
    app.helpers = Arc::new(HelperGate::new(1, 4));
    app.cfg.helper_queue_timeout_secs = 2;

    let mut item = movie_fixture(&app);
    item.detail_id = 9_100_003;
    item.object_id = "resize-descriptor-race-object".into();
    item.path = alias.clone();
    item.mime = "image/jpeg".into();
    item.ext = "jpg".into();
    item.size = std::fs::metadata(&inside).unwrap().len();
    {
        let mut catalog = write_recover(&app.catalog);
        catalog
            .by_detail
            .insert(item.detail_id, item.object_id.clone());
        catalog.items.insert(item.object_id.clone(), item);
    }

    let held = app.helpers.try_acquire().unwrap();
    let app = Arc::new(app);
    let worker_app = Arc::clone(&app);
    let worker = std::thread::spawn(move || {
        worker_app.handle(&req(&get(
            "/Resized/9100003.jpg?width=48,height=48",
            "RaceTest/1.0",
        )))
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while app.helpers.metrics().queued != 1 {
        assert!(
            Instant::now() < deadline,
            "resize did not enter helper queue"
        );
        std::thread::yield_now();
    }
    let replacement = root.join("replacement-link");
    std::os::unix::fs::symlink(&secret, &replacement).unwrap();
    std::fs::rename(replacement, &alias).unwrap();
    drop(held);

    let response = worker.join().unwrap();
    assert_eq!(
        response.status, 200,
        "authorized descriptor must remain usable"
    );
    assert!(response.body.starts_with(&[0xff, 0xd8, 0xff]));
    let later = app.handle(&req(&get(
        "/Resized/9100003.jpg?width=49,height=49",
        "RaceTest/1.0",
    )));
    assert_eq!(
        later.status, 404,
        "retargeted outside source must be rejected"
    );
}

fn soap_action(app: &App, action: &str, inner: &str, ua: &str) -> (u16, String) {
    let body = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">{inner}</u:{action}></s:Body></s:Envelope>"#
    );
    let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#{action}\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            body.len()
        );
    let mut req = HttpRequest::parse_headers(&raw).unwrap();
    req.body = body.into_bytes();
    let r = app.handle(&req);
    (r.status, String::from_utf8_lossy(&r.body).into_owned())
}

fn status_row_count(body: &str, label: &str) -> Option<u32> {
    let marker = format!("<tr><td>{label}</td><td>");
    let rest = body.split(&marker).nth(1)?;
    rest.split("</td>").next()?.parse().ok()
}

fn soap_browse(app: &App, oid: &str, flag: &str, ua: &str) -> (u16, String) {
    soap_browse_page(app, oid, flag, ua, 0, 0)
}

fn soap_browse_page(
    app: &App,
    oid: &str,
    flag: &str,
    ua: &str,
    start: usize,
    requested: usize,
) -> (u16, String) {
    let body = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>{oid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{requested}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
    );
    let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            body.len()
        );
    let mut req = HttpRequest::parse_headers(&raw).unwrap();
    req.body = body.into_bytes();
    let r = app.handle(&req);
    (r.status, String::from_utf8_lossy(&r.body).into_owned())
}

fn add_large_catalog_page(app: &App, count: usize) {
    let mut cat = write_recover(&app.catalog);
    let template = cat.items.values().next().cloned().expect("fixture item");
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let id = format!("2$8$B{index:X}");
        let mut item = template.clone();
        item.object_id = id.clone();
        item.parent_id = "2$8".into();
        item.detail_id = 1_000_000 + index as i64;
        item.title = format!("Bounded item {index:06} {}", "<&snow雪>".repeat(96));
        item.ref_id = None;
        cat.items.insert(id.clone(), item);
        ids.push(id);
    }
    cat.containers
        .get_mut("2$8")
        .expect("all-video container")
        .children
        .extend(ids);
}

#[test]
fn browse_root_and_kodi_original() {
    let app = testdata_app();
    let (st, xml) = soap_browse(&app, "0", "BrowseDirectChildren", "Kodi/21.0 (Linux)");
    assert_eq!(st, 200);
    assert!(xml.contains("&lt;DIDL-Lite"));
    assert!(xml.contains("id=\"64\"") || xml.contains("id=&quot;64&quot;"));
    assert!(xml.contains("id=\"1\"") || xml.contains("id=&quot;1&quot;"));
    assert!(xml.contains("id=\"2\"") || xml.contains("id=&quot;2&quot;"));
    let (stv, video) = soap_browse(&app, "2", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(stv, 200);
    assert!(video.contains("All Video"), "{video}");
    assert!(video.contains("Recently Added"), "{video}");
    assert!(video.contains("Folders"), "{video}");
    assert!(video.contains("Series"), "{video}");
    assert!(video.contains("Genre"), "{video}");
    assert!(
        video.contains("id=\"2$8\"") || video.contains("id=&quot;2$8&quot;"),
        "{video}"
    );
    assert!(
        video.contains("id=\"2$15\"") || video.contains("id=&quot;2$15&quot;"),
        "{video}"
    );
    assert!(
        video.contains("id=\"2$FF0\"") || video.contains("id=&quot;2$FF0&quot;"),
        "{video}"
    );
    assert!(
        video.contains("&lt;container ") && video.contains("storageUsed"),
        "folder DIDL (container + storageUsed) required for VLC expand: {video}"
    );
    assert!(video.contains("object.container.storageFolder"), "{video}");
    // items live under 2$8
    let (st2, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st2, 200);
    assert!(items.contains("/MediaItems/"));
    assert!(
        items.contains("size="),
        "res@size required for VLC: {items}"
    );
    assert!(
        items.contains("duration="),
        "res@duration H:MM:SS.mmm required for VLC length: {items}"
    );
    assert!(
        items.contains("dc:date&gt;") || items.contains("&lt;dc:date&gt;"),
        "missing dc:date in {items}"
    );
    // date is …Z or 10 chars
    assert!(
        items.contains("1999-01-01") || items.contains("Z&lt;/dc:date"),
        "date not normalized: {items}"
    );
    assert!(!items.contains("/Transcode/"), "Kodi must stay original");
}

#[test]
fn browse_uses_nfo_title_not_filename() {
    let app = testdata_app();
    let (st, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        items.contains("Fixture Movie"),
        "DIDL dc:title must use NFO title: {items}"
    );
    assert!(
        items.contains("A tiny spoiler-free fixture."),
        "DIDL dc:description must use the spoiler-safe outline: {items}"
    );
    assert!(
        !items.contains("A full fixture plot in which the ending is revealed."),
        "DIDL must not expose the spoiler plot: {items}"
    );
    assert!(
        !items.contains("&lt;dc:title&gt;movie&lt;/dc:title&gt;") && !items.contains(">movie<"),
        "filename-only title must not be the movie item title: {items}"
    );
}

#[test]
fn browse_series_seasons_and_genre() {
    let app = testdata_app();
    let (st, series) = soap_browse(&app, "2$E", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        series.contains("The Show"),
        "Series must list showtitle: {series}"
    );
    let show_id = {
        let cat = app.catalog.read().unwrap();
        cat.containers
            .values()
            .find(|c| c.parent_id == "2$E" && c.title == "The Show")
            .map(|c| c.object_id.clone())
            .expect("show container")
    };
    let (st, seasons) = soap_browse(&app, &show_id, "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(seasons.contains("Season 1"), "{seasons}");
    let season_id = {
        let cat = app.catalog.read().unwrap();
        cat.containers
            .values()
            .find(|c| c.parent_id == show_id && c.title == "Season 1")
            .map(|c| c.object_id.clone())
            .expect("season 1")
    };
    let (st, eps) = soap_browse(&app, &season_id, "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(eps.contains("Pilot"), "episode title under season: {eps}");
    assert!(eps.contains("/MediaItems/"), "{eps}");
    assert!(
        eps.contains("upnp:episodeSeason") || eps.contains("episodeSeason"),
        "{eps}"
    );
    let (st, genres) = soap_browse(&app, "2$9", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        genres.contains("Drama") || genres.contains("Crime"),
        "{genres}"
    );
    let drama_id = {
        let cat = app.catalog.read().unwrap();
        cat.containers
            .values()
            .find(|c| c.parent_id == "2$9" && (c.title == "Drama" || c.title == "Crime"))
            .map(|c| c.object_id.clone())
            .expect("genre folder")
    };
    let (st, items) = soap_browse(&app, &drama_id, "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        items.contains("The Show") || items.contains("Pilot"),
        "{items}"
    );
}

#[test]
fn album_art_get_and_didl() {
    let mut app = testdata_app();
    app.helpers = Arc::new(HelperGate::new(1, 1));
    app.cfg.helper_queue_timeout_secs = 1;
    let (st, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        items.contains("movie")
            || items.contains("Fixture Movie")
            || items.contains("&lt;dc:title&gt;movie"),
        "movie item missing: {items}"
    );
    assert!(
        items.contains("/AlbumArt/"),
        "DIDL missing /AlbumArt/: {items}"
    );
    assert!(
        items.contains("JPEG_TN") || items.contains("albumArtURI"),
        "DIDL missing JPEG_TN or albumArtURI: {items}"
    );
    let (art_id, detail_id) = parse_album_art_url(&items).expect("parse art url from DIDL");
    assert!(art_id > 0, "art id from DIDL");
    {
        let cat = read_recover(&app.catalog);
        let item = cat
            .items
            .values()
            .find(|item| item.detail_id == detail_id)
            .expect("DIDL artwork item in catalog");
        assert_eq!(art_id, item.album_art);
    }

    let r = app.handle(&req(&get(
        &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
        "Kodi/21.0",
    )));
    assert_eq!(r.status, 200, "album art GET");
    assert_eq!(resp_header(&r, "Content-Type"), Some("image/jpeg"));
    assert_eq!(
        resp_header(&r, "Cache-Control"),
        Some("private, max-age=86400")
    );
    assert!(
        r.body.len() >= 3 && r.body[0] == 0xff && r.body[1] == 0xd8,
        "JPEG magic"
    );
    assert_eq!(
        resp_header(&r, "transferMode.dlna.org"),
        Some("Interactive")
    );
    let feats = resp_header(&r, "contentFeatures.dlna.org").unwrap_or("");
    assert!(feats.contains("JPEG_TN"), "contentFeatures={feats}");

    let missing = app.handle(&req(&get("/AlbumArt/999999-1.jpg", "Kodi/21.0")));
    assert_eq!(missing.status, 404);

    let streaming = format!(
            "GET /AlbumArt/{art_id}-{detail_id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\ntransferMode.dlna.org: Streaming\r\n\r\n"
        );
    assert_eq!(app.handle(&req(&streaming)).status, 406);

    let ranged = format!(
            "GET /AlbumArt/{art_id}-{detail_id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-10\r\n\r\n"
        );
    assert_eq!(app.handle(&req(&ranged)).status, 406);

    let thumb = app.handle(&req(&get(
        &format!("/Thumbnails/{detail_id}.jpg"),
        "Kodi/21.0",
    )));
    assert_eq!(thumb.status, 200, "native thumb uses album art");
    assert_eq!(resp_header(&thumb, "Content-Type"), Some("image/jpeg"));
    let ffmpeg_ok = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-version"])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !ffmpeg_ok {
        eprintln!("skip /Resized/ GET (ffmpeg missing)");
    } else {
        let resized = app.handle(&req(&get(
            &format!("/Resized/{detail_id}.jpg?width=160,height=160"),
            "Kodi/21.0",
        )));
        assert_eq!(resized.status, 200, "resized GET");
        assert_eq!(resp_header(&resized, "Content-Type"), Some("image/jpeg"));
        assert_eq!(
            resp_header(&resized, "Cache-Control"),
            Some("private, max-age=86400")
        );
        assert_eq!(
            resp_header(&resized, "transferMode.dlna.org"),
            Some("Interactive")
        );
        assert!(
            resized.body.len() >= 2 && resized.body[0] == 0xff && resized.body[1] == 0xd8,
            "JPEG magic"
        );
        let feats = resp_header(&resized, "contentFeatures.dlna.org").unwrap_or("");
        assert!(feats.contains("JPEG_TN"), "contentFeatures={feats}");
        assert!(
            feats.contains("DLNA.ORG_CI=1"),
            "contentFeatures CI=1: {feats}"
        );

        let held = app.helpers.try_acquire().unwrap();
        let overloaded = app.handle(&req(&get(
            &format!("/Resized/{detail_id}.jpg?width=161,height=161"),
            "Kodi/21.0",
        )));
        assert_eq!(overloaded.status, 503);
        assert_eq!(resp_header(&overloaded, "Retry-After"), Some("1"));
        assert_eq!(app.helpers.metrics().timed_out_total, 1);
        drop(held);

        app.cfg.derived_image_cache_mb = 0;
        let cached_while_full = app.handle(&req(&get(
            &format!("/Resized/{detail_id}.jpg?width=160,height=160"),
            "Kodi/21.0",
        )));
        assert_eq!(
            cached_while_full.status, 200,
            "an existing derivative adds no storage and remains readable"
        );
        let over_quota = app.handle(&req(&get(
            &format!("/Resized/{detail_id}.jpg?width=162,height=162"),
            "Kodi/21.0",
        )));
        assert_eq!(over_quota.status, 507);
        let derived_dir = app.cache_dir.join("derived-images");
        assert!(
            std::fs::read_dir(derived_dir).unwrap().all(|entry| entry
                .unwrap()
                .path()
                .extension()
                .and_then(|value| value.to_str())
                != Some("jpg")),
            "a rejected over-quota image must not remain published"
        );
    }

    let xbox = app.handle(&req(&get(
        &format!("/MediaItems/{detail_id}.mkv?albumArt=true"),
        "Xbox/2.0.58767.0 UPnP/1.0 Xbox/2.0.58767.0",
    )));
    assert_eq!(xbox.status, 200);
    assert_eq!(resp_header(&xbox, "Content-Type"), Some("image/jpeg"));
    assert!(xbox.body.starts_with(&[0xff, 0xd8]));
}

#[test]
fn root_container_v_browse_zero_parent_is_root() {
    let mut app = testdata_app();
    app.cfg.root_container = Some("V".into());
    let (st, xml) = soap_browse(
        &app,
        "0",
        "BrowseDirectChildren",
        "VLC/3.0.21 LibVLC/3.0.21",
    );
    assert_eq!(st, 200);
    assert!(xml.contains("All Video"), "{xml}");
    assert!(xml.contains("Folders"), "{xml}");
    assert!(xml.contains("Recently Added"), "{xml}");
    assert!(xml.contains("Series"), "{xml}");
    assert!(xml.contains("Genre"), "{xml}");
    assert!(
        xml.contains("parentID=\"0\"") || xml.contains("parentID=&quot;0&quot;"),
        "remapped root advertises parentID=0: {xml}"
    );
    assert!(
        xml.contains("&lt;container ")
            && xml.contains("object.container.storageFolder")
            && xml.contains("storageUsed&gt;-1"),
        "VLC expand marker needs container + storageFolder + storageUsed: {xml}"
    );
    assert!(
        !xml.contains("&lt;item "),
        "root video view is folders only: {xml}"
    );
}

#[test]
fn browse_metadata_root_and_search() {
    let app = testdata_app();
    let (st, meta) = soap_browse(&app, "0", "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        meta.contains("parentID=\"-1\"") || meta.contains("parentID=&quot;-1&quot;"),
        "{meta}"
    );
    let body = r#"<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Search>"#;
    let raw = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:x#Search\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
    let mut req = HttpRequest::parse_headers(&raw).unwrap();
    req.body = body.as_bytes().to_vec();
    let r = app.handle(&req);
    assert_eq!(r.status, 200);
    let xml = String::from_utf8_lossy(&r.body);
    assert!(xml.contains("SearchResponse"));
    assert!(xml.contains("&lt;DIDL-Lite"));
    assert!(xml.contains("/MediaItems/"));
}

#[test]
fn large_browse_and_search_are_byte_and_page_bounded() {
    let mut app = testdata_app();
    // This fixture extends only the in-memory generation; production
    // requests use the SQLite query path when DB/catalog populations agree.
    app.scan_cfg.db_path = None;
    add_large_catalog_page(&app, 5_000);
    let expected_browse_total = app
        .catalog
        .read()
        .unwrap()
        .containers
        .get("2$8")
        .unwrap()
        .children
        .len() as u32;

    let started = std::time::Instant::now();
    let (status, browse) = soap_browse_page(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0", 0, 0);
    assert_eq!(status, 200);
    assert!(browse.len() <= MAX_SOAP_RESPONSE_BYTES, "{}", browse.len());
    let returned: u32 = xml_tag_text(&browse, "NumberReturned")
        .unwrap()
        .parse()
        .unwrap();
    let total: u32 = xml_tag_text(&browse, "TotalMatches")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(total, expected_browse_total);
    assert!(
        returned > 0 && returned < total,
        "returned={returned} total={total}"
    );
    assert!(returned as usize <= MAX_SOAP_PAGE_OBJECTS);

    let (_, page) = soap_browse_page(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0", 10, 3);
    assert_eq!(xml_tag_text(&page, "NumberReturned").as_deref(), Some("3"));
    assert_eq!(
        xml_tag_text(&page, "TotalMatches")
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        expected_browse_total
    );
    let (_, past_end) = soap_browse_page(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "Kodi/21.0",
        usize::try_from(i32::MAX).unwrap(),
        0,
    );
    assert_eq!(
        xml_tag_text(&past_end, "NumberReturned").as_deref(),
        Some("0")
    );
    assert_eq!(
        xml_tag_text(&past_end, "TotalMatches")
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        expected_browse_total
    );

    let (status, search) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains "Bounded item"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#,
        "Kodi/21.0",
    );
    assert_eq!(status, 200);
    assert!(search.len() <= MAX_SOAP_RESPONSE_BYTES, "{}", search.len());
    assert_eq!(
        xml_tag_text(&search, "TotalMatches").as_deref(),
        Some("5000")
    );
    let search_returned: u32 = xml_tag_text(&search, "NumberReturned")
        .unwrap()
        .parse()
        .unwrap();
    assert!(search_returned > 0 && search_returned < 5_000);

    let (_, search_past_end) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains "Bounded item"</SearchCriteria><Filter>*</Filter><StartingIndex>2147483647</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#,
        "Kodi/21.0",
    );
    assert_eq!(
        xml_tag_text(&search_past_end, "NumberReturned").as_deref(),
        Some("0")
    );
    assert_eq!(
        xml_tag_text(&search_past_end, "TotalMatches").as_deref(),
        Some("5000")
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "large-catalog regression took {:?}",
        started.elapsed()
    );
}

#[test]
fn pagination_totals_and_sort_stability_hold_for_every_small_page() {
    let app = testdata_app();
    let cat = app.catalog.read().unwrap();
    let parent = rusty_dlna_protocol::object_id::BROWSEDIR_ID;
    let (all, total) =
        sorted_child_page(&cat, parent, 0, usize::MAX, &[], DefaultOrder::FoldersFirst)
            .expect("Browse Folders container");
    assert_eq!(usize::try_from(total).unwrap(), all.len());
    for start in 0..=all.len().saturating_add(2) {
        for take in 0..=all.len().saturating_add(2) {
            let (page, page_total) =
                sorted_child_page(&cat, parent, start, take, &[], DefaultOrder::FoldersFirst)
                    .unwrap();
            assert_eq!(page_total, total);
            assert_eq!(page.len(), take.min(all.len().saturating_sub(start)));
            let page_ids: Vec<_> = page
                .iter()
                .map(|child| match child {
                    CatalogChild::Container(value) => value.object_id.as_str(),
                    CatalogChild::Item(value) => value.object_id.as_str(),
                })
                .collect();
            let expected_ids: Vec<_> = all
                .iter()
                .skip(start)
                .take(take)
                .map(|child| match child {
                    CatalogChild::Container(value) => value.object_id.as_str(),
                    CatalogChild::Item(value) => value.object_id.as_str(),
                })
                .collect();
            assert_eq!(page_ids, expected_ids);
        }
    }

    let template = cat.items.values().next().expect("fixture item").clone();
    drop(cat);
    let mut equal_keys = Vec::new();
    for id in ["stable-c", "stable-a", "stable-b"] {
        let mut item = template.clone();
        item.object_id = id.into();
        item.title = "same title".into();
        equal_keys.push(CatalogChild::Item(Box::new(item)));
    }
    sort_catalog_children(
        &mut equal_keys,
        &[SortSpec {
            key: SortKey::Title,
            descending: false,
        }],
        DefaultOrder::FoldersFirst,
    );
    let ids: Vec<_> = equal_keys
        .iter()
        .map(|child| match child {
            CatalogChild::Container(value) => value.object_id.as_str(),
            CatalogChild::Item(value) => value.object_id.as_str(),
        })
        .collect();
    assert_eq!(ids, ["stable-a", "stable-b", "stable-c"]);
}

#[test]
fn mixed_folder_and_item_sorting_uses_each_child_kinds_metadata() {
    let app = testdata_app();
    let cat = app.catalog.read().unwrap();
    let folder = cat
        .containers
        .values()
        .find(|container| container.object_id != rusty_dlna_protocol::object_id::ROOT_ID)
        .expect("fixture folder")
        .clone();
    let template = cat.items.values().next().expect("fixture item").clone();
    drop(cat);

    let mut alpha = template.clone();
    alpha.object_id = "sort-alpha".into();
    alpha.title = "Alpha item".into();
    alpha.date = "2020-01-01".into();
    alpha.album = Some("Alpha album".into());
    let mut zulu = template;
    zulu.object_id = "sort-zulu".into();
    zulu.title = "Zulu item".into();
    zulu.date = "2024-01-01".into();
    zulu.album = Some("Zulu album".into());

    let mut children = vec![
        CatalogChildRef::Item(&zulu),
        CatalogChildRef::Item(&alpha),
        CatalogChildRef::Container(&folder),
    ];
    sort_catalog_child_refs(&mut children, &[], DefaultOrder::FoldersFirst);
    assert!(matches!(children[0], CatalogChildRef::Container(_)));
    assert!(matches!(children[1], CatalogChildRef::Item(item) if item.object_id == "sort-alpha"));
    assert!(matches!(children[2], CatalogChildRef::Item(item) if item.object_id == "sort-zulu"));

    sort_catalog_child_refs(
        &mut children,
        &[SortSpec {
            key: SortKey::Date,
            descending: false,
        }],
        DefaultOrder::FoldersFirst,
    );
    assert!(matches!(children[0], CatalogChildRef::Container(_)));
    assert!(matches!(children[1], CatalogChildRef::Item(item) if item.object_id == "sort-alpha"));
    assert!(matches!(children[2], CatalogChildRef::Item(item) if item.object_id == "sort-zulu"));

    sort_catalog_child_refs(
        &mut children,
        &[SortSpec {
            key: SortKey::Album,
            descending: true,
        }],
        DefaultOrder::FoldersFirst,
    );
    assert!(matches!(children[0], CatalogChildRef::Item(item) if item.object_id == "sort-zulu"));
    assert!(matches!(children[1], CatalogChildRef::Item(item) if item.object_id == "sort-alpha"));
    assert!(matches!(children[2], CatalogChildRef::Container(_)));

    let mut tied_zulu = alpha.clone();
    tied_zulu.object_id = "sort-zulu-tie".into();
    tied_zulu.title = "Same title".into();
    let mut tied_alpha = alpha;
    tied_alpha.object_id = "sort-alpha-tie".into();
    tied_alpha.title = "Same title".into();
    let mut tied = vec![
        CatalogChildRef::Item(&tied_zulu),
        CatalogChildRef::Item(&tied_alpha),
    ];
    sort_catalog_child_refs(
        &mut tied,
        &[SortSpec {
            key: SortKey::Title,
            descending: true,
        }],
        DefaultOrder::FoldersFirst,
    );
    assert!(matches!(tied[0], CatalogChildRef::Item(item) if item.object_id == "sort-alpha-tie"));
    assert!(matches!(tied[1], CatalogChildRef::Item(item) if item.object_id == "sort-zulu-tie"));
}

#[test]
fn published_catalog_object_ids_are_globally_unique() {
    let app = testdata_app();
    let cat = app.catalog.read().unwrap();
    let mut ids = std::collections::HashSet::new();
    for id in cat.containers.keys().chain(cat.items.keys()) {
        assert!(ids.insert(id), "duplicate published object ID: {id}");
    }
    for (parent_id, container) in &cat.containers {
        let mut children = std::collections::HashSet::new();
        for child_id in &container.children {
            assert!(
                children.insert(child_id),
                "duplicate child {child_id} under {parent_id}"
            );
        }
    }
}

#[test]
fn request_time_sqlite_query_matches_the_published_catalog_generation() {
    let app = testdata_app();
    let query = CatalogQuery {
        groups: vec![vec![CatalogQueryClause {
            field: CatalogQueryField::Class,
            op: CatalogQueryOp::DerivedFrom("object.item.videoItem".into()),
        }]],
        sort: vec![CatalogQuerySort {
            field: CatalogQueryField::Title,
            descending: false,
        }],
        default_order: CatalogDefaultOrder::FoldersFirst,
    };
    let page = query_db_search(
        app.db_pool.as_deref(),
        app.scan_cfg.db_path.as_deref(),
        "0",
        &query,
        0,
        MAX_SOAP_PAGE_OBJECTS,
    )
    .expect("SQLite query path");
    let read_only_page = query_db_search(
        None,
        app.scan_cfg.db_path.as_deref(),
        "0",
        &query,
        0,
        MAX_SOAP_PAGE_OBJECTS,
    )
    .expect("read-only SQLite query path without a connection pool");
    assert_eq!(read_only_page.generation, page.generation);
    assert_eq!(read_only_page.page.object_ids, page.page.object_ids);
    let cat = app.catalog.read().unwrap();
    assert_eq!(page.page.population, catalog_population(&cat));
    let materialized = materialize_db_page(&cat, &page.page).expect("same DB/catalog generation");
    assert_eq!(materialized.len(), page.page.object_ids.len());
    assert!(page.page.total >= 1);
}

#[test]
fn sqlite_and_memory_tied_sort_pages_have_identical_stable_order() {
    let mut app = testdata_app();
    let clauses = rusty_dlna_soap::SearchQuery {
        groups: vec![vec![rusty_dlna_soap::SearchClause::All]],
    };
    let sort = [SortSpec {
        key: SortKey::Album,
        descending: false,
    }];
    let query = catalog_query(&clauses, &sort, DefaultOrder::FoldersFirst);
    let db_page = query_db_search(
        app.db_pool.as_deref(),
        app.scan_cfg.db_path.as_deref(),
        "0",
        &query,
        0,
        MAX_SOAP_PAGE_OBJECTS,
    )
    .expect("SQLite search page");
    let expected = db_page.page.object_ids;
    let cat = read_recover(&app.catalog);
    let client = identify_user_agent("DLNADOC/1.50").expect("generic DLNA profile");
    let (memory, total) = search_memory_page(
        &app,
        &cat,
        "0",
        &clauses,
        &sort,
        DefaultOrder::FoldersFirst,
        0,
        MAX_SOAP_PAGE_OBJECTS,
        client,
        Some("DLNADOC/1.50"),
        &FilterBits::default(),
    );
    let memory_ids = memory
        .iter()
        .map(|object| object.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(total as usize, expected.len());
    assert_eq!(memory_ids, expected, "SQLite and memory order diverged");
    assert!(
        memory.windows(2).any(|pair| pair[0].album == pair[1].album),
        "fixture must exercise a tied album key"
    );
    for (start, expected_id) in expected.iter().take(8).enumerate() {
        let db_single = query_db_search(
            app.db_pool.as_deref(),
            app.scan_cfg.db_path.as_deref(),
            "0",
            &query,
            start,
            1,
        )
        .expect("SQLite single-object page");
        assert_eq!(
            db_single.page.object_ids.as_slice(),
            std::slice::from_ref(expected_id)
        );
        let (memory_single, _) = search_memory_page(
            &app,
            &cat,
            "0",
            &clauses,
            &sort,
            DefaultOrder::FoldersFirst,
            start,
            1,
            client,
            Some("DLNADOC/1.50"),
            &FilterBits::default(),
        );
        assert_eq!(memory_single[0].id, *expected_id);
    }
    drop(cat);

    app.scan_cfg.db_path = None;
    for (start, expected_id) in expected.iter().take(8).enumerate() {
        let (status, xml) = soap_action(
            &app,
            "Search",
            &format!(
                "<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>1</RequestedCount><SortCriteria>+upnp:album</SortCriteria>"
            ),
            "DLNADOC/1.50",
        );
        assert_eq!(status, 200, "{xml}");
        let didl = xml_tag_text(&xml, "Result").expect("Search Result");
        assert!(
            didl.contains(&format!("id=\"{expected_id}\"")),
            "memory fallback page {start} did not contain {expected_id}: {didl}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_soap_client_disconnect_terminates_handler() {
    use tokio::io::AsyncWriteExt;

    let mut configured = testdata_app();
    configured.scan_cfg.db_path = None;
    let app = Arc::new(configured);
    add_large_catalog_page(&app, 5_000);
    let body = r#"<u:Browse><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
    let request = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_app = app.clone();
    let server = tokio::spawn(async move {
        let (socket, peer) = listener.accept().await.unwrap();
        handle_conn(server_app, socket, peer).await
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    drop(client);
    let outcome = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("disconnected SOAP handler did not terminate")
        .expect("handler task panicked");
    if let Err(error) = outcome {
        let text = error.to_string();
        assert!(
            text.contains("Broken pipe")
                || text.contains("reset")
                || text.contains("closed")
                || text.contains("Connection"),
            "unexpected disconnect error: {text}"
        );
    }
}

#[test]
fn missing_objectid_is_402_unknown_is_401() {
    let app = testdata_app();
    let body = r#"<s:Envelope><s:Body><u:Browse></u:Browse></s:Body></s:Envelope>"#;
    let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
    let mut req = HttpRequest::parse_headers(&raw).unwrap();
    req.body = body.as_bytes().to_vec();
    let r = app.handle(&req);
    assert_eq!(r.status, 500);
    let xml = String::from_utf8_lossy(&r.body);
    assert!(xml.contains("<errorCode>402</errorCode>"));

    let raw = "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Nope\"\r\nContent-Length: 0\r\n\r\n";
    let r = app.handle(&req_from(raw, b""));
    assert_eq!(r.status, 500);
    assert!(String::from_utf8_lossy(&r.body).contains("<errorCode>401</errorCode>"));
}

fn req_from(headers: &str, body: &[u8]) -> HttpRequest {
    let mut r = HttpRequest::parse_headers(headers).unwrap();
    r.body = body.to_vec();
    r
}

async fn raw_connection(app: Arc<App>, bytes: &[u8], close_write: bool) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, peer) = listener.accept().await.unwrap();
        handle_conn(app, socket, peer).await.unwrap();
    });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    if !bytes.is_empty() {
        client.write_all(bytes).await.unwrap();
    }
    if close_write {
        client.shutdown().await.unwrap();
    }
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), client.read_to_end(&mut response))
        .await
        .expect("server response timeout")
        .unwrap();
    server.await.unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_streaming_response_emits_only_the_fixed_500_fallback() {
    use std::io::Write;

    let mut app = testdata_app();
    let template = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie fixture");
    let test_tree = TestTree::new("invalid-streaming-response");
    let path = test_tree.path().join("sentinel.mkv");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"STREAM-SENTINEL").unwrap();
    file.set_len(8 * 1024 * 1024 + 1).unwrap();

    app.scan_cfg.media_roots.clear();
    app.scan_cfg.media_dirs = vec![test_tree.path().to_path_buf()];
    let mut item = template;
    item.object_id = "64$invalid-wire-stream".into();
    item.detail_id = 9_876_544;
    item.path = path;
    item.size = 8 * 1024 * 1024 + 1;
    item.mime = "video/mp4\r\nX-Injected: yes".into();
    {
        let mut catalog = app.catalog.write().unwrap();
        catalog
            .by_detail
            .insert(item.detail_id, item.object_id.clone());
        catalog.items.insert(item.object_id.clone(), item.clone());
    }

    let request = format!(
        "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n",
        item.detail_id
    );
    let response = raw_connection(Arc::new(app), request.as_bytes(), true).await;
    assert_eq!(
        response,
        b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    assert!(!response
        .windows(15)
        .any(|window| window == b"STREAM-SENTINEL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_parser_preserves_pipeline_and_rejects_smuggling() {
    let app = Arc::new(testdata_app());
    let pipeline = concat!(
        "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n"
    );
    let response = raw_connection(app.clone(), pipeline.as_bytes(), true).await;
    let text = String::from_utf8_lossy(&response);
    assert_eq!(
        text.matches("HTTP/1.1 200 OK").count(),
        2,
        "pipelined response bytes: {text}"
    );

    let body_pipeline = concat!(
        "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n",
        "Content-Length: 1\r\n\r\n",
        "x",
        "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n"
    );
    let response = raw_connection(app.clone(), body_pipeline.as_bytes(), true).await;
    let text = String::from_utf8_lossy(&response);
    assert_eq!(
        text.matches("HTTP/1.1 200 OK").count(),
        2,
        "body plus pipelined request was not preserved: {text}"
    );

    for malformed in [
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1, 1\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nTransfer-Encoding: identity\r\n\r\n",
            "GET / HTTP/1.1\r\nHost : 127.0.0.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nhOsT: 127.0.0.2\r\n\r\n",
            "GET / HTTP/9.9\r\nHost: 127.0.0.1\r\n\r\n",
        ] {
            let response = raw_connection(app.clone(), malformed.as_bytes(), true).await;
            assert!(
                response.starts_with(b"HTTP/1.1 400 Bad Request"),
                "malformed request was not 400: {}",
                String::from_utf8_lossy(&response)
            );
        }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soap_successes_and_faults_follow_http_keepalive() {
    let app = Arc::new(testdata_app());
    let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetSystemUpdateID xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/></s:Body></s:Envelope>"#;
    let first = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#GetSystemUpdateID\"\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
    let second = format!(
            "POST /ignored HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#GetSystemUpdateID\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    let success_pipeline = format!("{first}{second}");
    let response = raw_connection(app.clone(), success_pipeline.as_bytes(), true).await;
    let text = String::from_utf8_lossy(&response);
    assert_eq!(
        text.matches("HTTP/1.1 200 OK").count(),
        2,
        "SOAP success did not persist: {text}"
    );
    assert!(
        text.contains("Connection: keep-alive"),
        "first SOAP response was not persistent: {text}"
    );

    let fault = concat!(
        "POST /ignored HTTP/1.1\r\n",
        "Host: 127.0.0.1:18200\r\n",
        "SOAPAction: \"urn:x#NoSuchAction\"\r\n",
        "Content-Length: 0\r\n\r\n"
    );
    let fault_pipeline = format!("{fault}{second}");
    let response = raw_connection(app.clone(), fault_pipeline.as_bytes(), true).await;
    let text = String::from_utf8_lossy(&response);
    assert_eq!(
        text.matches("HTTP/1.1 500 Internal Server Error").count(),
        1
    );
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 1);
    assert!(text.contains("Connection: keep-alive"));

    let http10_keep = HttpRequest::parse_headers(&format!(
            "POST /ignored HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#GetSystemUpdateID\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ))
        .unwrap();
    let mut http10_keep = http10_keep;
    http10_keep.body = body.as_bytes().to_vec();
    assert!(app.handle(&http10_keep).persist);

    let mut http10_close = http10_keep.clone();
    http10_close
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("Connection"));
    assert!(!app.handle(&http10_close).persist);

    let rejected_chunked = raw_connection(
        app,
        b"POST /ignored HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        true,
    )
    .await;
    assert!(rejected_chunked.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert_eq!(
        String::from_utf8_lossy(&rejected_chunked)
            .matches("HTTP/1.1")
            .count(),
        1,
        "unsupported chunked request must close the connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_body_caps_incomplete_bodies_and_slow_headers_are_bounded() {
    let mut configured = testdata_app();
    configured.cfg.max_request_body_bytes = 8;
    configured.cfg.header_read_timeout_secs = 1;
    configured.cfg.body_read_timeout_secs = 1;
    let app = Arc::new(configured);

    let oversized = raw_connection(
        app.clone(),
        b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 9\r\n\r\n",
        true,
    )
    .await;
    assert!(oversized.starts_with(b"HTTP/1.1 413 Payload Too Large"));

    let incomplete = raw_connection(
        app.clone(),
        b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\nab",
        true,
    )
    .await;
    assert!(incomplete.starts_with(b"HTTP/1.1 400 Bad Request"));

    let started = std::time::Instant::now();
    let timeout = raw_connection(app, b"G", false).await;
    assert!(timeout.starts_with(b"HTTP/1.1 408 Request Timeout"));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_connections_holds_excess_clients_in_the_kernel_backlog() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut configured = testdata_app();
    configured.cfg.max_connections = 1;
    let app = Arc::new(configured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(accept_loop(listener, app));

    let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
    first.write_all(b"G").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
    second
        .write_all(
            b"GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut byte = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), second.read(&mut byte))
            .await
            .is_err(),
        "second connection was serviced while the sole permit was occupied"
    );

    first.shutdown().await.unwrap();
    drop(first);
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), second.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    server.abort();
}

#[test]
fn original_get_and_two_ranges() {
    let app = testdata_app();
    let movie = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|i| i.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie fixture");
    let id = movie.detail_id;
    let path = movie.path.clone();
    let expect = std::fs::read(&path).unwrap();
    let raw = format!(
            "GET /MediaItems/{id}.ignored HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        );
    let r = app.handle(&req(&raw));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, expect);
    let hdrs = r
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(hdrs.contains("Accept-Ranges: bytes"));
    assert!(hdrs.contains("DLNA.ORG_OP=01"));
    assert!(hdrs.contains("DLNA.ORG_CI=0"));
    assert!(!r.persist);

    let r1 = app.handle(&req(&format!(
        "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-99\r\n\r\n"
    )));
    assert_eq!(r1.status, 206);
    assert_eq!(r1.body, expect[0..100]);
    let r2 = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=100-199\r\n\r\n"
        )));
    assert_eq!(r2.status, 206);
    assert_eq!(r2.body, expect[100..200]);

    let bad = app.handle(&req(&format!(
        "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=abc\r\n\r\n"
    )));
    assert_eq!(bad.status, 400);
    let past = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=9999999-99999999\r\n\r\n"
        )));
    assert_eq!(past.status, 416);
}

#[cfg(unix)]
#[test]
fn original_get_opens_non_utf8_catalog_path_without_loss() {
    use std::os::unix::ffi::OsStringExt;

    let mut app = testdata_app();
    let template = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie fixture");
    let test_tree = TestTree::new("http-nonutf8");
    let dir = test_tree.path().to_path_buf();
    let mut raw_name = b"movie-".to_vec();
    raw_name.push(0x80);
    raw_name.extend_from_slice(b".mkv");
    let path = dir.join(std::ffi::OsString::from_vec(raw_name));
    let expected = b"non-utf8 media body";
    std::fs::write(&path, expected).unwrap();
    app.scan_cfg.media_roots.clear();
    app.scan_cfg.media_dirs = vec![dir.clone()];
    let mut item = template;
    item.object_id = "64$nonutf8".into();
    item.detail_id = 9_876_543;
    item.path = path;
    item.size = expected.len() as u64;
    {
        let mut catalog = app.catalog.write().unwrap();
        catalog
            .by_detail
            .insert(item.detail_id, item.object_id.clone());
        catalog.items.insert(item.object_id.clone(), item.clone());
    }

    let response = app.handle(&req(&format!(
        "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        item.detail_id
    )));
    assert_eq!(response.status, 200);
    assert_eq!(response.body, expected);
}

fn assert_head_has_no_payload(r: &HttpResponse) {
    assert!(r.body.is_empty(), "HEAD retained an in-memory body");
    assert!(r.file_range.is_none(), "HEAD retained a file stream");
    assert!(r.remux_job.is_none(), "HEAD retained a remux stream");
    let wire = r.bytes_wire("test", "Thu, 01 Jan 1970 00:00:00 GMT");
    let split = wire
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header terminator");
    assert_eq!(&wire[split + 4..], b"", "HEAD emitted wire body bytes");
}

#[test]
fn head_suppresses_every_media_payload_without_changing_metadata() {
    let mut app = testdata_app();
    let movie = movie_fixture(&app);
    let id = movie.detail_id;

    let small = app.handle(&req(&format!(
        "HEAD /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
    )));
    assert_eq!(small.status, 200);
    assert_eq!(
        resp_header(&small, "Content-Length"),
        Some(movie.size.to_string().as_str())
    );
    assert_head_has_no_payload(&small);

    let ranged = app.handle(&req(&format!(
        "HEAD /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-15\r\n\r\n"
    )));
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.reason, "Partial Content");
    assert_eq!(resp_header(&ranged, "Content-Length"), Some("16"));
    assert_eq!(
        resp_header(&ranged, "Content-Range"),
        Some(format!("bytes 0-15/{}", movie.size).as_str())
    );
    let ranged_wire = ranged
        .try_bytes_wire("test", "Thu, 01 Jan 1970 00:00:00 GMT")
        .unwrap();
    assert!(ranged_wire.starts_with(b"HTTP/1.1 206 Partial Content\r\n"));
    assert!(ranged_wire.ends_with(b"\r\n\r\n"));
    assert_head_has_no_payload(&ranged);

    // A sparse file crosses the streaming threshold without allocating a
    // large test buffer. HEAD must not leave a deferred file stream.
    let big_path = app.cache_dir.join("head-large.mkv");
    std::fs::create_dir_all(&app.cache_dir).unwrap();
    let big_size = 9 * 1024 * 1024;
    std::fs::File::create(&big_path)
        .unwrap()
        .set_len(big_size)
        .unwrap();
    app.scan_cfg.media_dirs.push(app.cache_dir.clone());
    let mut big = movie.clone();
    big.detail_id = 9_000_001;
    big.object_id = "head-large-object".into();
    big.path = big_path.clone();
    big.size = big_size;
    {
        let mut cat = app.catalog.write().unwrap();
        cat.by_detail.insert(big.detail_id, big.object_id.clone());
        cat.items.insert(big.object_id.clone(), big.clone());
    }
    let large = app.handle(&req(&format!(
        "HEAD /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        big.detail_id
    )));
    assert_eq!(large.status, 200);
    assert_eq!(
        resp_header(&large, "Content-Length"),
        Some(big_size.to_string().as_str())
    );
    assert_head_has_no_payload(&large);

    let transcode = {
        let dvp7 = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.path.ends_with("dvp7.mkv"))
            .cloned()
            .expect("dvp7 fixture");
        app.handle(&req(&format!(
                "HEAD /Transcode/{}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54.384650 DLNADOC/1.50\r\n\r\n",
                dvp7.detail_id
            )))
    };
    assert_eq!(transcode.status, 200);
    assert_head_has_no_payload(&transcode);

    let art = movie.album_art;
    assert!(art > 0);
    let art_head = app.handle(&req(&format!(
        "HEAD /AlbumArt/{art}-{id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
    )));
    assert_eq!(art_head.status, 200);
    assert_head_has_no_payload(&art_head);

    let caption = movie.captions.first().expect("caption fixture");
    let caption_head = app.handle(&req(&format!(
        "HEAD /Captions/{id}/{}.{} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        caption.index, caption.ext
    )));
    assert_eq!(caption_head.status, 200);
    assert_head_has_no_payload(&caption_head);

    let missing = app.handle(&req(
        "HEAD /MediaItems/999999.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
    ));
    assert_eq!(missing.status, 404);
    assert_head_has_no_payload(&missing);
    let _ = std::fs::remove_file(big_path);
}

#[test]
fn transcode_get_without_remap_serves_original() {
    let app = testdata_app();
    let movie = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|i| i.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie fixture");
    let id = movie.detail_id;
    let expect = std::fs::read(&movie.path).unwrap();
    let r = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        )));
    assert_eq!(r.status, 200, "must not 404 a guessed remux URL");
    assert!(r.remux_job.is_none(), "SDR original is not remuxed");
    assert_eq!(r.body, expect);
}

#[test]
fn crkey_dvp7_remap_first_kodi_original() {
    let app = testdata_app();
    let dvp7 = dvp7_fixture(&app);
    let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert!(kodi.contains(&format!("/MediaItems/{}.mkv", dvp7.detail_id)));
    // testdata remap is CrKey-only, so Kodi still sees the original.
    assert!(!kodi.contains(&format!("/Transcode/{}.mp4", dvp7.detail_id)));
    let (_, cr) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "CrKey/1.54.384650 DLNADOC/1.50",
    );
    assert_transcode_before_original(&cr, dvp7.detail_id);
    assert!(cr.contains("DLNA.ORG_CI=1"));
}

#[test]
fn transcode_config_gates_jobs_and_controls_encoder_and_audio() {
    let mut app = testdata_app();
    let dvp7 = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("dvp7.mkv"))
        .cloned()
        .expect("dvp7 fixture");
    let id = dvp7.detail_id;
    let crkey = "CrKey/1.54.384650 DLNADOC/1.50";

    app.cfg.transcode.enable = false;
    let (_, disabled_didl) = soap_browse(&app, "2$8", "BrowseDirectChildren", crkey);
    assert!(
        !disabled_didl.contains("/Transcode/"),
        "disabled transcode leaked into DIDL: {disabled_didl}"
    );
    let disabled = app.handle(&req(&format!(
        "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
    )));
    assert_eq!(disabled.status, 200);
    assert!(disabled.remux_job.is_none());
    assert_eq!(disabled.body, std::fs::read(&dvp7.path).unwrap());

    app.cfg.transcode.enable = true;
    app.cfg.transcode.encoder = "libx264".into();
    app.remaps = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "hdr10"
audio_out = "copy"
"#,
    )
    .unwrap();
    let global_default = app.handle(&req(&format!(
        "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
    )));
    let global_spec = global_default.remux_job.expect("global encoder job");
    assert!(global_spec.continue_after_disconnect);
    assert!(
        global_spec.args.iter().any(|arg| arg == "libx264"),
        "global encoder missing: {:?}",
        global_spec.args
    );

    app.remaps = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "hdr10"
encoder = "hevc_nvenc"
"#,
    )
    .unwrap();
    let override_resp = app.handle(&req(&format!(
        "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
    )));
    let override_spec = override_resp.remux_job.expect("rule encoder job");
    assert!(override_spec.args.iter().any(|arg| arg == "hevc_nvenc"));
    assert!(!override_spec.args.iter().any(|arg| arg == "libx264"));

    for (configured, expected) in [
        ("copy", RemuxAudio::Copy),
        ("to-ac3", RemuxAudio::Ac3),
        ("to-aac", RemuxAudio::Aac),
    ] {
        app.remaps = rusty_dlna_transcode::parse_remaps_toml(&format!(
            r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "{configured}"
"#
        ))
        .unwrap();
        let response = app.handle(&req(&format!(
                "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
        )));
        let spec = response.remux_job.expect("Profile-8 job");
        assert_eq!(spec.audio, expected, "audio_out={configured}");
        if dovi_tool_path().is_none() {
            assert!(
                !spec.remux_p8,
                "missing dovi_tool must select HDR10 upfront"
            );
            assert!(spec.args.iter().any(|arg| arg == "libx264"));
        }
    }
}

#[test]
fn kodi_p7_remap_advertises_transcode() {
    let mut app = testdata_app();
    app.remaps = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
name = "kodi-dvp7"
client = "Kodi"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
    )
    .unwrap();
    let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_transcode_before_original(&kodi, dvp7_fixture(&app).detail_id);
    let (_, plat) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13",
    );
    assert!(
        plat.contains("/Transcode/"),
        "Platinum UA must match Kodi remap: {plat}"
    );
    assert!(kodi.contains("DLNA.ORG_CI=1"));
    let t0 = std::time::Instant::now();
    let (_, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(200),
        "Browse must not ffprobe: {:?}",
        t0.elapsed()
    );
    assert!(xml.contains("/Transcode/"), "{xml}");
}

fn feature_list(app: &App, ua: &str) -> String {
    let raw = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:x#X_GetFeatureList\"\r\nContent-Length: 0\r\n\r\n"
        );
    let r = app.handle(&req(&raw));
    String::from_utf8_lossy(&r.body).into_owned()
}

#[test]
fn client_matrix_handlers() {
    let app = testdata_app();
    // Kodi: original MKV res, date Z or 10 chars, caption <res>
    let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert!(kodi.contains("/MediaItems/"));
    assert!(!kodi.contains("/Transcode/"));
    assert!(kodi.contains("1999-01-01") || kodi.contains("Z&lt;/dc:date"));
    assert!(
        kodi.contains("/Captions/"),
        "FLAG_CAPTION_RES Kodi Browse must list /Captions/: {kodi}"
    );

    // SEC_HHP_[PC] is not Samsung BASICVIEW
    let pc = app.handle(&req(&get(
        "/rootDesc.xml",
        "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
    )));
    assert!(!String::from_utf8_lossy(&pc.body).contains("sec:ProductCap"));
    let pc_fl = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0");
    assert!(pc_fl.contains("id=&quot;1&quot;"), "{pc_fl}");
    assert!(pc_fl.contains("id=&quot;2&quot;"), "{pc_fl}");
    assert!(pc_fl.contains("id=&quot;3&quot;"), "{pc_fl}");
    assert!(
        !pc_fl.contains("id=&quot;A&quot;"),
        "PC must not use A: {pc_fl}"
    );
    assert!(
        !pc_fl.contains("id=&quot;V&quot;"),
        "PC must not use V: {pc_fl}"
    );
    assert!(
        !pc_fl.contains("id=&quot;I&quot;"),
        "PC must not use I: {pc_fl}"
    );
    let (_, pc_didl) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
    );
    assert!(
        !pc_didl.contains("sec:CaptionInfoEx") && !pc_didl.contains("xmlns:sec"),
        "AllShare must stay non-Samsung: {pc_didl}"
    );

    // SEC_HHP_[TV] → A/V/I + x-mkv
    let tv_fl = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
    assert!(tv_fl.contains("id=&quot;A&quot;") && tv_fl.contains("id=&quot;V&quot;"));
    let (_, tv) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0",
    );
    assert!(tv.contains("video/x-mkv"), "{tv}");

    // [BD]J5500: FLAG_SKIP_DLNA_PN — media protocolInfo has no PN.
    // Album-art JPEG_TN <res> is still emitted (Phase 11).
    let (_, j5500) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "DLNADOC/1.50 [BD]J5500",
    );
    assert!(
        !j5500.contains("video/x-matroska:DLNA.ORG_PN=")
            && !j5500.contains("video/x-mkv:DLNA.ORG_PN="),
        "J5500 media res must skip DLNA.ORG_PN: {j5500}"
    );

    // Xbox rootDesc modelNumber=1
    let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/360")));
    assert!(String::from_utf8_lossy(&xbox.body).contains("<modelNumber>1</modelNumber>"));

    // CrKey: transcode res first on DV P7
    let (_, cr) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "CrKey/1.54 DLNADOC/1.50",
    );
    assert_transcode_before_original(&cr, dvp7_fixture(&app).detail_id);
    assert!(cr.contains("DLNA.ORG_CI=1"));

    // Generic DLNADOC/1.50 is not NEED_SAFE_VIDEO
    let generic = identify_user_agent("DLNADOC/1.50 UPnP/1.0").unwrap();
    assert!(!generic.flags.contains(ClientFlags::NEED_SAFE_VIDEO));
    let (_, gen) = soap_browse(&app, "2$8", "BrowseDirectChildren", "DLNADOC/1.50");
    assert!(!gen.contains("/Transcode/"));
}

fn movie_fixture(app: &App) -> rusty_dlna_scan::MediaItem {
    let cat = app.catalog.read().unwrap();
    let any = cat
        .items
        .values()
        .find(|i| i.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie.mkv fixture");
    cat.get_item_by_detail(any.detail_id)
        .cloned()
        .unwrap_or(any)
}

fn dvp7_fixture(app: &App) -> rusty_dlna_scan::MediaItem {
    app.catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("dvp7.mkv"))
        .cloned()
        .expect("dvp7 fixture")
}

fn assert_transcode_before_original(xml: &str, detail_id: i64) {
    let transcode = format!("/Transcode/{detail_id}.mp4");
    let original = format!("/MediaItems/{detail_id}.mkv");
    let transcode_pos = xml
        .find(&transcode)
        .unwrap_or_else(|| panic!("DIDL missing {transcode}: {xml}"));
    let original_pos = xml
        .find(&original)
        .unwrap_or_else(|| panic!("DIDL missing {original}: {xml}"));
    assert!(
        transcode_pos < original_pos,
        "transcode resource must precede the matching original: {xml}"
    );
}

fn set_detail_dlna_pn(app: &App, detail_id: i64, pn: &str) {
    let mut cat = write_recover(&app.catalog);
    let oid = cat
        .by_detail
        .get(&detail_id)
        .cloned()
        .expect("by_detail oid");
    cat.items.get_mut(&oid).expect("by_detail item").dlna_pn = Some(pn.into());
}

#[test]
fn setbookmark_then_browse_position() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let (st, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("X_SetBookmarkResponse"), "{xml}");

    let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
            || xml.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
        "lastPlaybackPosition 120 missing: {xml}"
    );

    let dbp = app.scan_cfg.db_path.as_ref().expect("testdata db_path");
    let db = LibraryDb::open(dbp).unwrap();
    let got = db.get_bookmark(movie.detail_id).unwrap();
    assert_eq!(
        got.map(|(s, _)| s),
        Some(120),
        "BOOKMARKS.SEC for detail {}",
        movie.detail_id
    );
}

#[test]
fn bookmark_waiting_for_database_writer_does_not_block_browse() {
    let app = Arc::new(testdata_app());
    let movie = movie_fixture(&app);
    let pool = app.db_pool.as_ref().unwrap();
    let writer = pool
        .writer
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let before = app.update_id.load(Ordering::Relaxed);
    let bookmark_object_id = movie.object_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();

    let publishing_app = Arc::clone(&app);
    let bookmark = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        soap_action(
            &publishing_app,
            "X_SetBookmark",
            &format!("<ObjectID>{bookmark_object_id}</ObjectID><PosSecond>321</PosSecond>"),
            "Kodi/21.0",
        )
    });
    started_rx.recv().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match app.catalog_publication.try_lock() {
            Err(std::sync::TryLockError::WouldBlock) => break,
            Err(std::sync::TryLockError::Poisoned(error)) => {
                panic!("catalog publication mutex poisoned: {error}")
            }
            Ok(guard) => drop(guard),
        }
        assert!(
            Instant::now() < deadline,
            "bookmark did not reach database writer wait"
        );
        std::thread::yield_now();
    }

    assert!(
        app.catalog.try_read().is_ok(),
        "database writer contention must not hold the catalog write lock"
    );
    let (status, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(status, 200, "{xml}");
    assert!(
        !xml.contains("lastPlaybackPosition&gt;321"),
        "the unpublished bookmark must not be visible: {xml}"
    );

    drop(writer);
    let (status, xml) = bookmark.join().unwrap();
    assert_eq!(status, 200, "{xml}");

    let after = app.update_id.load(Ordering::Relaxed);
    assert_eq!(after, before.saturating_add(1));
    assert!(app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .items
        .values()
        .filter(|item| item.detail_id == movie.detail_id)
        .all(|item| item.bookmark_sec == 321));
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((321, 0)));
    assert_eq!(db.get_update_id().unwrap(), after);
}

#[test]
fn samsung_q_bookmark_convert_ms_bm() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let q = "SEC_HHP_[TV] Samsung Q";
    let (st, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120000</PosSecond>",
            movie.object_id
        ),
        q,
    );
    assert_eq!(st, 200, "{xml}");

    let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", q);
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("&lt;upnp:lastPlaybackPosition&gt;120000&lt;/upnp:lastPlaybackPosition&gt;")
            || xml.contains("<upnp:lastPlaybackPosition>120000</upnp:lastPlaybackPosition>"),
        "lastPlaybackPosition 120000 missing: {xml}"
    );
    assert!(
        xml.contains("BM=120000"),
        "dcmInfo BM=120000 missing: {xml}"
    );

    let (st, kodi) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{kodi}");
    assert!(
        kodi.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
            || kodi.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
        "Kodi lastPlaybackPosition 120 missing: {kodi}"
    );
    assert!(
        !kodi.contains("120000"),
        "Kodi must keep stored seconds, not ms: {kodi}"
    );
}

#[test]
fn kodi_soap_object_id_normalization_is_strict() {
    assert_eq!(
        normalize_soap_object_id("64%241%245%244%241/"),
        Some("64$1$5$4$1".into())
    );
    assert_eq!(normalize_soap_object_id("64%241%2f"), Some("64$1".into()));
    assert_eq!(normalize_soap_object_id("64$1$5"), Some("64$1$5".into()));
    for invalid in ["64%", "64%2", "64%GG", "64%00$1"] {
        assert_eq!(normalize_soap_object_id(invalid), None, "{invalid}");
    }
    assert_eq!(normalize_soap_object_id(&"x".repeat(1025)), None);
}

#[test]
fn kodi_encoded_updateobject_persists_and_browses_resume_position() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let kodi_oid = format!("{}/", movie.object_id.replace('$', "%24"));
    // Kodi's SaveFileState always sends the opaque xbmc:lastPlayerState
    // extension beside a changed resume point. The opaque content must not
    // be interpreted as part of the writable DIDL property list.
    let current_tags = "&lt;upnp:lastPlaybackPosition&gt;0&lt;/upnp:lastPlaybackPosition&gt;,\
                            &lt;xbmc:lastPlayerState&gt;&amp;opaque-current;,old&lt;/xbmc:lastPlayerState&gt;";
    let new_tags = "&lt;upnp:lastPlaybackPosition&gt;00:02:00&lt;/upnp:lastPlaybackPosition&gt;,\
                        &lt;xbmc:lastPlayerState&gt;&amp;opaque-new;,new&lt;/xbmc:lastPlayerState&gt;";
    let kodi_ua = "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13";
    let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{kodi_oid}</ObjectID><CurrentTagValue>{current_tags}</CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>"
            ),
            kodi_ua,
        );
    assert_eq!(status, 200, "{xml}");
    assert!(xml.contains("UpdateObjectResponse"), "{xml}");

    let (status, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", kodi_ua);
    assert_eq!(status, 200, "{xml}");
    assert!(
        xml.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
            || xml.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
        "resume position missing from Browse response: {xml}"
    );
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
}

#[test]
fn kodi_bookmark_update_invalidates_every_cached_parent_container() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let expected_parents = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .filter(|item| item.detail_id == movie.detail_id)
        .map(|item| item.parent_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!expected_parents.is_empty());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let subscribe = req(&format!(
        "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 127.0.0.1:18200\r\n\
             Callback: <http://127.0.0.1:{}/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        addr.port()
    ));
    assert_eq!(app.handle_from(&subscribe, addr).status, 200);
    let initial = accept_notify(&listener, std::time::Duration::from_secs(3));
    assert!(initial.contains("<ContainerUpdateIDs></ContainerUpdateIDs>"));

    let before = app.update_id.load(Ordering::Relaxed);
    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13",
    );
    assert_eq!(status, 200, "{xml}");
    let after = app.update_id.load(Ordering::Relaxed);
    assert_eq!(after, before.saturating_add(1));

    let notify = accept_notify(&listener, std::time::Duration::from_secs(3));
    assert!(
        notify.contains(&format!("<SystemUpdateID>{after}</SystemUpdateID>")),
        "{notify}"
    );
    for parent in expected_parents {
        assert!(
            notify.contains(&format!("{parent},{after}")),
            "parent {parent:?} missing from {notify}"
        );
    }
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_update_id().unwrap(), after);
}

#[test]
fn bookmark_database_failure_returns_action_failed_without_catalog_drift() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let db_path = app.scan_cfg.db_path.as_ref().unwrap();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_bookmark_write BEFORE INSERT ON BOOKMARKS
             BEGIN SELECT RAISE(FAIL, 'forced bookmark failure'); END;",
    )
    .unwrap();
    drop(conn);

    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>501</errorCode>"), "{xml}");
    assert_eq!(
        app.catalog
            .read()
            .unwrap()
            .get_item_by_detail(movie.detail_id)
            .unwrap()
            .bookmark_sec,
        0,
        "the in-memory catalog must not claim a write SQLite rejected"
    );
    let db = LibraryDb::open(db_path).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
}

#[test]
fn updateobject_playcount_and_position() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let new_tags = "&lt;upnp:playCount&gt;3&lt;/upnp:playCount&gt;&lt;upnp:lastPlaybackPosition&gt;90&lt;/upnp:lastPlaybackPosition&gt;";
    let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("UpdateObjectResponse"), "{xml}");

    let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("&lt;upnp:playbackCount&gt;3&lt;/upnp:playbackCount&gt;")
            || xml.contains("<upnp:playbackCount>3</upnp:playbackCount>"),
        "playbackCount 3 missing: {xml}"
    );
    assert!(
        xml.contains("&lt;upnp:lastPlaybackPosition&gt;90&lt;/upnp:lastPlaybackPosition&gt;")
            || xml.contains("<upnp:lastPlaybackPosition>90</upnp:lastPlaybackPosition>"),
        "lastPlaybackPosition 90 missing: {xml}"
    );

    let (st, xml) = soap_action(
        &app,
        "UpdateObject",
        "<CurrentTagValue></CurrentTagValue><NewTagValue></NewTagValue>",
        "Kodi/21.0",
    );
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");

    let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            "<ObjectID>999999</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>&lt;upnp:playCount&gt;1&lt;/upnp:playCount&gt;</NewTagValue>",
            "Kodi/21.0",
        );
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
}

#[test]
fn updateobject_minus_one_clears_bookmark() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let (st, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(st, 200, "{xml}");

    let new_tags = "&lt;upnp:lastPlaybackPosition&gt;-1&lt;/upnp:lastPlaybackPosition&gt;";
    let current_tags = "&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;";
    let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>{current_tags}</CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("UpdateObjectResponse"), "{xml}");

    let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    assert!(
        !xml.contains("lastPlaybackPosition"),
        "cleared bookmark must omit lastPlaybackPosition: {xml}"
    );

    let dbp = app.scan_cfg.db_path.as_ref().expect("testdata db_path");
    let db = LibraryDb::open(dbp).unwrap();
    let got = db.get_bookmark(movie.detail_id).unwrap();
    assert_eq!(
        got.map(|(s, _)| s),
        Some(0),
        "BOOKMARKS.SEC must be 0 after lastPlaybackPosition=-1"
    );
}

#[test]
fn setbookmark_missing_position_is_402_without_clearing_state() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");
    let update_id = app.update_id.load(Ordering::Relaxed);

    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!("<ObjectID>{}</ObjectID>", movie.object_id),
        "Kodi/21.0",
    );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    assert_eq!(app.update_id.load(Ordering::Relaxed), update_id);
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
}

#[test]
fn updateobject_rejects_missing_malformed_and_unsupported_tag_arguments() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let oid = &movie.object_id;
    let cases = [
            (
                format!(
                    "<ObjectID>{oid}</ObjectID><NewTagValue>upnp:playCount=1</NewTagValue>"
                ),
                402,
            ),
            (
                format!(
                    "<ObjectID>{oid}</ObjectID><CurrentTagValue>upnp:playCount=0</CurrentTagValue>"
                ),
                402,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>broken</CurrentTagValue><NewTagValue>upnp:playCount=1</NewTagValue>"),
                702,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>broken</NewTagValue>"),
                703,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>dc:title=Old</CurrentTagValue><NewTagValue>dc:title=New</NewTagValue>"),
                705,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>upnp:playCount=0</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>"),
                706,
            ),
        ];
    for (body, code) in cases {
        let (status, xml) = soap_action(&app, "UpdateObject", &body, "Kodi/21.0");
        assert_eq!(status, 500, "code {code}: {xml}");
        assert!(
            xml.contains(&format!("<errorCode>{code}</errorCode>")),
            "{xml}"
        );
    }
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
}

#[test]
fn updateobject_current_value_is_an_optimistic_concurrency_guard() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let initial_update_id = app.update_id.load(Ordering::Relaxed);
    let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=1</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>702</errorCode>"), "{xml}");
    assert_eq!(app.update_id.load(Ordering::Relaxed), initial_update_id);
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
    drop(db);

    let (status, xml) = soap_action(
        &app,
        "X_SetBookmark",
        &format!(
            "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
            movie.object_id
        ),
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");
    let update_id = app.update_id.load(Ordering::Relaxed);

    let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=60</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>702</errorCode>"), "{xml}");
    assert_eq!(app.update_id.load(Ordering::Relaxed), update_id);
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
    drop(db);

    let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=120</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
    assert_eq!(status, 200, "{xml}");
    let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((90, 0)));
}

/// True if a DIDL `<res>` body (escaped or raw) points at `/Captions/`.
fn didl_res_has_captions(xml: &str) -> bool {
    for (open, close) in [("&lt;res", "&lt;/res&gt;"), ("<res", "</res>")] {
        let mut rest = xml;
        while let Some(start) = rest.find(open) {
            rest = &rest[start..];
            let Some(end) = rest.find(close) else {
                break;
            };
            if rest[..end].contains("/Captions/") {
                return true;
            }
            rest = &rest[end + close.len()..];
        }
    }
    false
}

#[test]
fn samsung_captioninfoex_and_header() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    assert!(
        !movie.captions.is_empty(),
        "movie.mkv must have sidecar captions"
    );
    let ua = "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0";
    let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", ua);
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("sec:CaptionInfoEx"), "{xml}");
    assert!(xml.contains("xmlns:sec"), "{xml}");
    assert!(xml.contains("/Captions/"), "{xml}");
    assert!(
        xml.contains(&format!("/Captions/{}/", movie.detail_id))
            || xml.contains(&format!("/Captions/{}.srt", movie.detail_id)),
        "CaptionInfoEx URL missing for detail {}: {xml}",
        movie.detail_id
    );

    let raw = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\ngetCaptionInfo.sec: 1\r\n\r\n",
            movie.detail_id
        );
    let r = app.handle(&req(&raw));
    assert_eq!(r.status, 200, "media GET");
    let hdr = resp_header(&r, "CaptionInfo.sec").unwrap_or("");
    assert!(
        hdr.contains(&format!("/Captions/{}.srt", movie.detail_id)),
        "CaptionInfo.sec={hdr}"
    );
}

#[test]
fn kodi_caption_res_no_sec_by_default() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("/Captions/"), "{xml}");
    assert!(
        xml.contains(&format!("/Captions/{}/0.srt", movie.detail_id))
            || xml.contains(&format!("/Captions/{}/1.srt", movie.detail_id)),
        "Kodi caption <res> must be indexed /Captions/{{id}}/n.srt: {xml}"
    );
    assert!(!xml.contains("sec:CaptionInfoEx"), "{xml}");
    assert!(!xml.contains("xmlns:sec"), "{xml}");
    assert!(!xml.contains("xmlns:pv"), "{xml}");
    assert!(!xml.contains("pv:subtitle"), "{xml}");
    assert!(didl_res_has_captions(&xml), "{xml}");
}

#[test]
fn samsung_bdp_no_caption_res() {
    let app = testdata_app();
    let ua = "DLNADOC/1.50 SEC_HHP_BD-D5100/1.0";
    let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", ua);
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("/MediaItems/"),
        "BDP still lists media URLs: {xml}"
    );
    assert!(
        !didl_res_has_captions(&xml),
        "SEC_HHP_BD must not get caption <res> (folder-browse bug): {xml}"
    );
}

#[test]
fn soap_caps_and_unknown_path_browse() {
    let app = testdata_app();
    for (action, needle) in [
        ("GetSearchCapabilities", "dc:title"),
        ("GetSortCapabilities", "dc:date"),
        ("GetProtocolInfo", "video/x-matroska"),
        ("GetSystemUpdateID", "<Id>"),
    ] {
        let raw = format!(
                "POST /not-a-real-path HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#{action}\"\r\nContent-Length: 0\r\n\r\n"
            );
        let r = app.handle(&req(&raw));
        assert_eq!(r.status, 200, "{action}");
        assert!(
            String::from_utf8_lossy(&r.body).contains(needle),
            "{action} {}",
            String::from_utf8_lossy(&r.body)
        );
    }
}

#[test]
fn transcode_get_serves_growing_file_before_completion() {
    let mut app = testdata_app();
    app.remaps = rusty_dlna_transcode::parse_remaps_toml(
        r#"
[[remap]]
name = "kodi-dvp7"
client = "Kodi"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
    )
    .unwrap();
    let dvp7 = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|i| i.path.ends_with("dvp7.mkv"))
        .cloned()
        .expect("dvp7");
    let id = dvp7.detail_id;
    let t0 = std::time::Instant::now();
    let r = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13\r\n\r\n"
        )));
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(500),
        "GET /Transcode must not wait for a full remux ({:?})",
        t0.elapsed()
    );
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty(), "body is streamed after headers");
    let spec = r.remux_job.expect("background remux job");
    assert_eq!(spec.detail_id, id);
    assert!(spec.verified_ffmpeg.is_some());
    assert_eq!(spec.args[0], "ffmpeg");
    assert!(!spec.args.iter().any(|s| s == "pipe:1"), "{:?}", spec.args);
    assert!(
        spec.args
            .iter()
            .any(|s| s.to_string_lossy().contains("frag_keyframe")),
        "{:?}",
        spec.args
    );
    assert!(
        !spec
            .args
            .iter()
            .any(|s| s.to_string_lossy().contains("faststart")),
        "{:?}",
        spec.args
    );
    assert!(
        spec.args
            .iter()
            .any(|s| s.to_string_lossy().ends_with(".part")),
        "must write a .part file: {:?}",
        spec.args
    );
    let hdrs = r
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!hdrs.to_ascii_lowercase().contains("accept-ranges"));
    assert!(!hdrs.to_ascii_lowercase().contains("content-length"));
    assert!(hdrs.contains("DLNA.ORG_OP=00"), "{hdrs}");
    assert!(hdrs.contains("DLNA.ORG_CI=1"), "{hdrs}");
    let dest = spec.dest.clone();
    assert!(
        !dest.is_file(),
        "handle() must not wait for a finished cache"
    );
    if dovi_tool_path().is_some() {
        assert!(spec.remux_p8, "available dovi_tool must attempt Profile-8");
    } else {
        assert!(
            !spec.remux_p8,
            "missing dovi_tool must select HDR10 upfront"
        );
        assert!(spec.args.iter().any(|argument| argument == "libx264"));
    }

    let kodi_orig = testdata_app();
    let miss = kodi_orig.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        )));
    assert_eq!(miss.status, 200, "no remap → original, not 404");
    assert!(miss.remux_job.is_none());
}

#[test]
fn remux_finished_range_and_stale_rebuild() {
    use std::io::Read;
    use std::sync::Arc;
    let app = testdata_app();
    let dvp7 = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|i| i.path.ends_with("dvp7.mkv"))
        .cloned()
        .expect("dvp7");
    let live = rusty_dlna_scan::rebase_media_path_for_config(&dvp7.path, &app.scan_cfg);
    let src = app.cache_dir.join("dvp7-stamp-src.mkv");
    let _ = std::fs::copy(&live, &src);
    let cache_key = rusty_dlna_transcode::source_identity(&src).unwrap();
    let dest = rusty_dlna_transcode::cache_dest_for_key(
        &app.cache_dir,
        dvp7.detail_id,
        rusty_dlna_transcode::RecodeAction::RemuxP8,
        &cache_key,
    );
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let payload = b"0123456789abcdefFINISHED_REMUX_BYTES";
    std::fs::write(&dest, payload).unwrap();
    rusty_dlna_transcode::write_cache_stamp_for_key(&dest, &cache_key).unwrap();
    assert!(rusty_dlna_transcode::cache_is_fresh_for_key(
        &dest, &cache_key
    ));

    let spec = RemuxJobSpec {
        detail_id: dvp7.detail_id,
        web_session_id: None,
        web_request_id: None,
        mime: "video/mp4",
        job_key: format!("{}:{cache_key}:fixture", dvp7.detail_id),
        cache_key: cache_key.clone(),
        src: src.clone(),
        source_file: None,
        ai_upscale_shader_file: None,
        dest: dest.clone(),
        args: vec!["ffmpeg".into(), "-version".into()],
        fallback_args: None,
        continue_after_disconnect: true,
        cacheable: true,
        hls_all_fragments_independent: false,
        remux_p8: true,
        verified_ffmpeg: None,
        profile8_toolchain: None,
        audio_index: 0,
        audio: RemuxAudio::Copy,
    };
    let req = HttpRequest::parse_headers(
        "GET /Transcode/1.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-15\r\n\r\n",
    )
    .unwrap();
    let app = Arc::new(app);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (hdr, body) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app2 = app.clone();
        let spec2 = spec.clone();
        let h = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            remux::serve_remux(&app2, &mut sock, &req, spec2)
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let mut buf = Vec::new();
        let _ = c.read_to_end(&mut buf);
        let _ = h.await;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(buf.len());
        let body = if split + 4 <= buf.len() {
            buf[split + 4..].to_vec()
        } else {
            Vec::new()
        };
        (text, body)
    });
    assert!(hdr.contains("206") || hdr.contains("HTTP/1.1 206"), "{hdr}");
    assert!(
        hdr.to_ascii_lowercase().contains("accept-ranges: bytes"),
        "{hdr}"
    );
    assert!(hdr.contains("DLNA.ORG_OP=01"), "{hdr}");
    assert_eq!(body, &payload[..16]);

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&src, b"replaced-source-bytes-to-bust-stamp").unwrap();
    let replaced_key = rusty_dlna_transcode::source_identity(&src).unwrap();
    assert_ne!(replaced_key, cache_key, "source identity must change");
    assert!(
        !rusty_dlna_transcode::cache_is_fresh_for_key(&dest, &replaced_key),
        "replaced source must invalidate remux dest"
    );
}

#[test]
fn empty_remap_is_original_for_everyone() {
    let mut app = testdata_app();
    app.remaps.clear();
    let (_, cr) = soap_browse(&app, "2$8", "BrowseDirectChildren", "CrKey/1.54");
    assert!(!cr.contains("/Transcode/"));
}

fn response_content_type(r: &HttpResponse) -> &str {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

fn body_is_png(body: &[u8]) -> bool {
    body.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

fn body_is_jpeg(body: &[u8]) -> bool {
    body.len() >= 3 && body[0] == 0xff && body[1] == 0xd8 && body[2] == 0xff
}

fn png_dimensions(body: &[u8]) -> Option<(u32, u32)> {
    body.get(16..24).map(|dimensions| {
        (
            u32::from_be_bytes(dimensions[..4].try_into().unwrap()),
            u32::from_be_bytes(dimensions[4..].try_into().unwrap()),
        )
    })
}

fn jpeg_dimensions(body: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2usize;
    while offset + 8 < body.len() {
        if body[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = body[offset + 1];
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return Some((
                u16::from_be_bytes([body[offset + 7], body[offset + 8]]) as u32,
                u16::from_be_bytes([body[offset + 5], body[offset + 6]]) as u32,
            ));
        }
        let length = u16::from_be_bytes([body[offset + 2], body[offset + 3]]) as usize;
        if length < 2 {
            return None;
        }
        offset = offset.checked_add(length + 2)?;
    }
    None
}

#[test]
fn icon_png_and_jpg_magic_matches_content_type() {
    let app = testdata_app();
    let png = app.handle(&req(&get("/icons/sm.png", "Kodi/21.0")));
    assert_eq!(png.status, 200);
    let png_ct = response_content_type(&png);
    assert!(
        png_ct.eq_ignore_ascii_case("image/png"),
        "png Content-Type={png_ct}"
    );
    assert!(
        body_is_png(&png.body),
        "png body missing PNG magic: {:02x?}",
        &png.body[..png.body.len().min(8)]
    );

    let jpg = app.handle(&req(&get("/icons/sm.jpg", "Kodi/21.0")));
    assert_eq!(jpg.status, 200);
    let jpg_ct = response_content_type(&jpg);
    assert!(
        jpg_ct.eq_ignore_ascii_case("image/jpeg"),
        "jpg Content-Type={jpg_ct}"
    );
    assert!(
        body_is_jpeg(&jpg.body),
        "jpg body missing JPEG SOI: {:02x?}",
        &jpg.body[..jpg.body.len().min(8)]
    );
    assert!(
        !body_is_png(&jpg.body),
        "image/jpeg response must not be PNG bytes"
    );

    assert_eq!(png_dimensions(&png.body), Some((48, 48)));
    assert_eq!(jpeg_dimensions(&jpg.body), Some((48, 48)));
    let large_png = app.handle(&req(&get("/icons/lrg.png", "Kodi/21.0")));
    let large_jpeg = app.handle(&req(&get("/icons/lrg.jpg", "Kodi/21.0")));
    assert_eq!(png_dimensions(&large_png.body), Some((120, 120)));
    assert_eq!(jpeg_dimensions(&large_jpeg.body), Some((120, 120)));
}

#[test]
fn derived_image_keys_and_cache_limits_prevent_stale_unbounded_files() {
    let test_tree = TestTree::new("derived-cache");
    let cache = test_tree.path().to_path_buf();
    let key_a = derived_image_key("source-a", 160, 160, 2, 0);
    let key_b = derived_image_key("source-b", 160, 160, 2, 0);
    assert_eq!(key_a.len(), 64);
    assert_ne!(key_a, key_b, "source replacement must change the cache key");
    assert_ne!(
        key_a,
        derived_image_key("source-a", 640, 480, 2, 0),
        "geometry is part of the cache key"
    );

    let old = cache.join(format!("{key_a}.jpg"));
    let new = cache.join(format!("{key_b}.jpg"));
    std::fs::write(&old, vec![1u8; 800]).unwrap();
    std::fs::write(&new, vec![2u8; 800]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    prune_derived_image_cache(&cache, 900, 36_500, 0).unwrap();
    assert!(!old.exists(), "oldest entry is evicted first");
    assert!(new.exists());
}

#[test]
fn soap_faults_are_500_upnperror_with_request_persistence() {
    let app = testdata_app();

    let body = r#"<s:Envelope><s:Body><u:Browse></u:Browse></s:Body></s:Envelope>"#;
    let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
    let r402 = app.handle(&req_from(&raw, body.as_bytes()));
    assert_eq!(r402.status, 500);
    let xml402 = String::from_utf8_lossy(&r402.body);
    assert!(xml402.contains("UPnPError"), "{xml402}");
    assert!(xml402.contains("<errorCode>402</errorCode>"), "{xml402}");
    assert!(r402.persist);

    let r401 = app.handle(&req_from(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Nope\"\r\nContent-Length: 0\r\n\r\n",
            b"",
        ));
    assert_eq!(r401.status, 500);
    let xml401 = String::from_utf8_lossy(&r401.body);
    assert!(xml401.contains("UPnPError"), "{xml401}");
    assert!(xml401.contains("<errorCode>401</errorCode>"), "{xml401}");
    assert!(r401.persist);

    let (st701, xml701) = soap_browse(&app, "no-such-object", "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st701, 500);
    assert!(xml701.contains("UPnPError"), "{xml701}");
    assert!(xml701.contains("<errorCode>701</errorCode>"), "{xml701}");
    let r701 = {
        let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>no-such-object</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
        let raw = format!(
                "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                body.len()
            );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = body.into_bytes();
        app.handle(&req)
    };
    assert!(r701.persist);
}

#[test]
fn browse_metadata_accepts_detail_id_and_all_video_hex() {
    let app = testdata_app();
    let movie = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|i| i.title == "movie" || i.path.ends_with("movie.mkv"))
        .cloned()
        .expect("movie fixture");
    let did = movie.detail_id;
    let all_video = format!("2$8${did:X}");
    let (st, xml) = soap_browse(
        &app,
        &all_video,
        "BrowseMetadata",
        "Darwin/25.6.0, UPnP/1.0, Portable SDK for UPnP devices/1.14.13",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("/MediaItems/"), "{xml}");
    // Bare DETAILS.ID only when it cannot be a virtual container (`0`/`1`/`2`/`3`/`64`).
    if did > 64 {
        let (st2, xml2) = soap_browse(&app, &did.to_string(), "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st2, 200, "{xml2}");
        assert!(xml2.contains("/MediaItems/"), "{xml2}");
    }
}

#[test]
fn two_typed_media_dirs_keep_both_classes() {
    let test_tree = TestTree::new("two-media-dirs");
    let tmp = test_tree.path().to_path_buf();
    let vdir = tmp.join("video");
    let adir = tmp.join("audio");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::create_dir_all(&adir).unwrap();
    rusty_dlna_scan::write_fake_mkv(&vdir.join("clip.mkv"), 64);
    let mut flac = b"fLaC".to_vec();
    flac.extend_from_slice(&[0u8; 48]);
    std::fs::write(adir.join("song.flac"), flac).unwrap();
    let cfg = Config {
        friendly_name: "twodir".into(),
        media_dir: vec![
            format!("V,{}", vdir.display()),
            format!("A,{}", adir.display()),
        ],
        cache_dir: Some(tmp.join("cache").display().to_string()),
        rescan_secs: 0,
        ..Config::default()
    };
    let app = App::from_config(cfg, 18200, 11900, &tmp);
    assert!(
        app.scan_cfg.types.video,
        "V, prefix must survive a later A, (got {:?})",
        app.scan_cfg.types
    );
    assert!(
        app.scan_cfg.types.audio,
        "A, prefix must remain (got {:?})",
        app.scan_cfg.types
    );
    let cat = scan(&app.scan_cfg).unwrap();
    let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
    assert!(
        titles.contains(&"clip"),
        "video under V dir must be accepted: {titles:?}"
    );
    assert!(
        titles.contains(&"song"),
        "audio under A dir must be accepted: {titles:?}"
    );
}

#[test]
fn every_supported_format_agrees_across_scan_browse_and_get() {
    let test_tree = TestTree::new("formats");
    let tmp = test_tree.path().to_path_buf();
    let media = tmp.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let video_fixture = tmp.join("fixture.mkv");
    rusty_dlna_scan::write_fake_mkv(&video_fixture, 64);
    let audio_fixture = tmp.join("fixture.wav");
    let mut wav = Vec::from(b"RIFF".as_slice());
    wav.extend_from_slice(&36u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&8_000u32.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(&audio_fixture, wav).unwrap();

    let mut expected = Vec::new();
    for (index, format) in rusty_dlna_protocol::MEDIA_FORMATS.iter().enumerate() {
        let resolved = format.resolve(None);
        let path = media.join(format!("format-{index}.{}", format.extension));
        match resolved.kind {
            rusty_dlna_protocol::MediaKind::Video => {
                std::fs::copy(&video_fixture, &path).unwrap();
            }
            rusty_dlna_protocol::MediaKind::Audio => {
                std::fs::copy(&audio_fixture, &path).unwrap();
            }
            rusty_dlna_protocol::MediaKind::Image => {
                std::fs::write(&path, TINY_JPEG).unwrap();
            }
        }
        expected.push((path, resolved));
        if format.is_ambiguous() {
            let path = media.join(format!("audio-only-{index}.{}", format.extension));
            std::fs::copy(&audio_fixture, &path).unwrap();
            expected.push((
                path,
                format.resolve(Some(rusty_dlna_protocol::MediaKind::Audio)),
            ));
        }
    }

    let cfg = Config {
        friendly_name: "format-map".into(),
        media_dir: vec![media.display().to_string()],
        cache_dir: Some(tmp.join("cache").display().to_string()),
        thumbnails: false,
        rescan_secs: 0,
        ..Config::default()
    };
    let app = App::from_config(cfg, 18200, 11900, &tmp);
    *app.catalog.write().unwrap() = scan(&app.scan_cfg).unwrap();
    for (path, resolved) in expected {
        let item = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.path == path)
            .cloned()
            .unwrap_or_else(|| panic!("{} was not indexed", path.display()));
        assert_eq!(item.mime, resolved.mime, "{}", path.display());
        assert_eq!(item.class, resolved.upnp_class(), "{}", path.display());

        let response = app.handle(&req(&get(
            &format!("/MediaItems/{}.{}", item.detail_id, item.ext),
            "FormatMapTest/1.0",
        )));
        assert_eq!(response.status, 200, "GET {}", path.display());
        let get_mime = resp_header(&response, "Content-Type")
            .unwrap_or_else(|| panic!("GET {} has no Content-Type", path.display()));

        let (status, didl) =
            soap_browse(&app, &item.object_id, "BrowseMetadata", "FormatMapTest/1.0");
        assert_eq!(status, 200, "Browse {}: {didl}", path.display());
        assert!(
            didl.contains(get_mime),
            "Browse {} did not advertise GET MIME {}: {didl}",
            path.display(),
            get_mime
        );
    }
}

#[test]
fn status_lists_video_count() {
    let app = testdata_app();
    let r = app.handle(&req(&get("/status", "Kodi/21.0")));
    assert_eq!(r.status, 200);
    assert_eq!(
        resp_header(&r, "Content-Type"),
        Some("text/html; charset=utf-8")
    );
    let body = String::from_utf8_lossy(&r.body);
    assert!(body.contains("Video"), "{body}");
    let video_n = status_row_count(&body, "Video").expect("Video row");
    assert!(video_n >= 1, "Video count {video_n}: {body}");
    let audio_n = status_row_count(&body, "Audio").expect("Audio row");
    assert!(audio_n >= 1, "Audio count {audio_n}: {body}");
    let image_n = status_row_count(&body, "Image").expect("Image row");
    assert!(image_n >= 1, "Image count {image_n}: {body}");
    assert!(
        body.contains("refresh") || body.contains("Refresh") || body.contains("20"),
        "{body}"
    );
    assert!(
        !body.contains("<H1>200 OK</H1>"),
        "status must be the document, not the error-page wrapper: {body}"
    );
    assert!(
        body.contains("<h1>rustyDLNA-test</h1>") || body.contains("<title>rustyDLNA-test</title>"),
        "{body}"
    );
    let root = app.handle(&req(&get("/", "Kodi/21.0")));
    assert_eq!(root.status, 200);
    let root_body = String::from_utf8_lossy(&root.body);
    assert!(
        root_body.contains("<title>rustyDLNA</title>"),
        "{root_body}"
    );
    assert!(root_body.contains("/web/app.js"), "{root_body}");
}

#[test]
fn status_html_uses_the_shared_complete_markup_escape() {
    let mut app = testdata_app();
    app.cfg.friendly_name = "Name & <tag> > \"quoted\" 'apostrophe' \u{1}".into();
    let body = status::status_html(&app);

    assert!(
        body.contains(
            "Name &amp; &lt;tag&gt; &gt; &quot;quoted&quot; &apos;apostrophe&apos; \u{fffd}"
        ),
        "{body}"
    );
    assert!(!body.contains("<tag>"), "{body}");
    assert!(!body.contains('\u{1}'), "{body}");
}

#[test]
fn web_player_is_embedded_searchable_and_independently_disabled() {
    let mut app = testdata_app();

    let css = app.handle(&req(&get("/web/app.css", "Browser/1.0")));
    assert_eq!(css.status, 200);
    assert_eq!(
        resp_header(&css, "Content-Type"),
        Some("text/css; charset=utf-8")
    );
    assert_eq!(resp_header(&css, "Cache-Control"), Some("no-cache"));
    let favicon = app.handle(&req(&get("/favicon.ico", "Browser/1.0")));
    assert_eq!(favicon.status, 200);
    assert_eq!(resp_header(&favicon, "Content-Type"), Some("image/png"));
    assert_eq!(
        resp_header(&favicon, "Cache-Control"),
        Some("public, max-age=86400")
    );
    assert!(favicon.body.starts_with(b"\x89PNG\r\n\x1a\n"));
    for asset in [
        "/web/app.js",
        "/web/api.js",
        "/web/core.js",
        "/web/library.js",
        "/web/player.js",
        "/web/preferences.js",
        "/web/store.js",
    ] {
        let response = app.handle(&req(&get(asset, "Browser/1.0")));
        assert_eq!(response.status, 200, "{asset}");
        assert_eq!(
            resp_header(&response, "Content-Type"),
            Some("text/javascript; charset=utf-8"),
            "{asset}"
        );
        assert_eq!(resp_header(&response, "Cache-Control"), Some("no-cache"));
    }
    assert!(css.body.starts_with(b":root"));

    let root = app.handle(&req(&get("/", "Browser/1.0")));
    let root_html = String::from_utf8_lossy(&root.body);
    assert!(root_html.contains("data-view=\"folders\""), "{root_html}");
    assert!(
        root_html.contains("id=\"playback-controls\""),
        "{root_html}"
    );
    assert!(root_html.contains("href=\"/favicon.ico\""), "{root_html}");
    assert!(!root_html.contains("?v="), "{root_html}");
    assert!(root_html.contains("id=\"timeline-status\""), "{root_html}");
    assert!(root_html.contains("id=\"volume-control\""), "{root_html}");
    assert!(!root_html.contains("data-seek="), "{root_html}");
    assert!(
        root_html.contains("class=\"library-results\""),
        "{root_html}"
    );
    assert!(root_html.contains("role=\"tabpanel\""), "{root_html}");
    assert!(root_html.contains("<select id=\"audio-track-controls\""));
    let stage_start = root_html.find("id=\"player-stage\"").unwrap();
    let stage_end = root_html[stage_start..]
        .find("<div class=\"player-message\"")
        .unwrap()
        + stage_start;
    let stage = &root_html[stage_start..stage_end];
    for control in [
        "close-player-button",
        "timeline",
        "volume-control",
        "stream-info-button",
        "captions-button",
        "audio-track-controls",
        "fullscreen-button",
    ] {
        assert!(
            stage.contains(&format!("id=\"{control}\"")),
            "{control} must be inside fullscreen stage"
        );
    }

    let folders = app.handle(&req(&get(
        "/api/web/library?view=folders&folder=64&offset=0&limit=200",
        "Browser/1.0",
    )));
    assert_eq!(folders.status, 200);
    let folders_etag = resp_header(&folders, "ETag").unwrap().to_owned();
    assert!(folders_etag.starts_with("W/\"web-v2-r5-"), "{folders_etag}");
    let stale_capability_etag = folders_etag.replacen("-r5-", "-r4-", 1);
    let stale_conditional = req(&format!(
        "GET /api/web/library?view=folders&folder=64&offset=0&limit=200 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\nIf-None-Match: {stale_capability_etag}\r\n\r\n"
    ));
    assert_eq!(app.handle(&stale_conditional).status, 200);
    assert_eq!(
        resp_header(&folders, "Cache-Control"),
        Some("private, max-age=0, must-revalidate")
    );
    let conditional = req(&format!(
        "GET /api/web/library?view=folders&folder=64&offset=0&limit=200 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\nIf-None-Match: {folders_etag}\r\n\r\n"
    ));
    let conditional = app.handle(&conditional);
    assert_eq!(conditional.status, 304);
    assert!(conditional.body.is_empty());
    let folders: serde_json::Value = serde_json::from_slice(&folders.body).unwrap();
    assert_eq!(folders["schema_version"], 2);
    assert!(folders["generation"].is_u64());
    assert_eq!(folders["root_folder_id"], "64");
    assert_eq!(folders["capabilities"]["resume"], "browser_local");
    assert_eq!(folders["capabilities"]["transcoding"], true);
    let profiles = folders["capabilities"]["quality_profiles"]
        .as_array()
        .expect("quality profiles");
    assert_eq!(profiles.len(), 7);
    let presets = folders["capabilities"]["encoding_presets"]
        .as_array()
        .unwrap();
    assert_eq!(
        presets
            .iter()
            .map(|preset| preset["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["balanced", "fast_start", "maximum_speed"]
    );
    assert_eq!(profiles[0]["id"], "auto");
    assert_eq!(profiles[0]["max_width"], 3840);
    assert_eq!(profiles[0]["max_height"], 2160);
    assert_eq!(profiles[0]["max_fps"], 30);
    assert_eq!(profiles[0]["h264_level"], "5.1");
    assert_eq!(profiles[0]["max_video_kbps"], 25000);
    assert_eq!(profiles[1]["id"], "uhd_high");
    assert_eq!(profiles[1]["max_video_kbps"], 25000);
    assert_eq!(profiles[2]["id"], "uhd_optimized");
    assert_eq!(profiles[2]["max_video_kbps"], 16000);
    assert_eq!(profiles[3]["id"], "full_hd");
    assert_eq!(profiles[3]["max_video_kbps"], 8000);
    assert_eq!(profiles[4]["id"], "data_saver");
    assert_eq!(profiles[4]["max_video_kbps"], 3000);
    assert_eq!(profiles[4]["automatic_fallback"], true);
    assert_eq!(profiles[5]["id"], "sd_480");
    assert_eq!(profiles[5]["max_video_kbps"], 1500);
    assert_eq!(profiles[6]["id"], "low_360");
    assert_eq!(profiles[6]["max_video_kbps"], 800);
    let video_outputs = folders["capabilities"]["video_outputs"]
        .as_array()
        .expect("video outputs");
    assert_eq!(video_outputs.len(), 1);
    assert_eq!(video_outputs[0]["id"], "h264_sdr");
    assert_eq!(folders["view"], "folders");
    assert_eq!(folders["folder"]["id"], "64");
    assert_eq!(folders["breadcrumbs"][0]["title"], "Media");
    let first_folder = folders["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["entry_type"] == "folder")
        .expect("fixture media root folder");
    assert!(first_folder["child_count"].is_number());
    let first_folder_id = first_folder["id"].as_str().unwrap();
    let child = app.handle(&req(&get(
        &format!("/api/web/library?view=folders&folder={first_folder_id}&offset=0&limit=200"),
        "Browser/1.0",
    )));
    assert_eq!(child.status, 200);
    let child: serde_json::Value = serde_json::from_slice(&child.body).unwrap();
    assert_eq!(child["breadcrumbs"].as_array().unwrap().len(), 2);
    assert!(!child["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        app.handle(&req(&get(
            "/api/web/library?view=folders&folder=2",
            "Browser/1.0"
        )))
        .status,
        404
    );
    for invalid in [
        "/api/web/library?view=unknown",
        "/api/web/library?view=library&kind=photos",
        "/api/web/library?view=library&offset=-1",
        "/api/web/library?view=library&limit=201",
        "/api/web/library?view=library&surprise=true",
    ] {
        let response = app.handle(&req(&get(invalid, "Browser/1.0")));
        assert_eq!(response.status, 400, "{invalid}");
        let error: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error["schema_version"], 2);
        assert!(error["error"]["code"].is_string());
        assert!(error["error"]["message"].is_string());
    }
    let stale_generation = folders["generation"].as_u64().unwrap().saturating_add(1);
    let stale = app.handle(&req(&get(
        &format!("/api/web/library?view=library&generation={stale_generation}"),
        "Browser/1.0",
    )));
    assert_eq!(stale.status, 409);
    let stale: serde_json::Value = serde_json::from_slice(&stale.body).unwrap();
    assert_eq!(stale["error"]["code"], "catalog_changed");

    let duplicate_template = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("movie.mkv"))
        .cloned()
        .unwrap();
    {
        let mut catalog = app.catalog.write().unwrap();
        for detail_id in [9_200_001, 9_200_002] {
            let mut duplicate = duplicate_template.clone();
            duplicate.detail_id = detail_id;
            duplicate.object_id = format!("web-alias-{detail_id}");
            duplicate.title = "Web Alias Duplicate".into();
            catalog
                .by_detail
                .insert(detail_id, duplicate.object_id.clone());
            catalog.items.insert(duplicate.object_id.clone(), duplicate);
        }
    }
    let deduplicated = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=Web%20Alias%20Duplicate&limit=10",
        "Browser/1.0",
    )));
    let deduplicated: serde_json::Value = serde_json::from_slice(&deduplicated.body).unwrap();
    assert_eq!(deduplicated["total"], 1);

    let exact_large_id = 9_007_199_254_740_993i64;
    {
        let mut large = duplicate_template.clone();
        large.detail_id = exact_large_id;
        large.object_id = "web-large-decimal-id".into();
        large.title = "Exact Large Decimal ID".into();
        large.path = large.path.with_file_name("exact-large-decimal-id.mkv");
        let mut catalog = app.catalog.write().unwrap();
        catalog
            .by_detail
            .insert(exact_large_id, large.object_id.clone());
        catalog.items.insert(large.object_id.clone(), large);
    }
    let large_id_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=Exact%20Large%20Decimal%20ID&limit=10",
        "Browser/1.0",
    )));
    assert_eq!(large_id_page.status, 200);
    let large_id_page: serde_json::Value = serde_json::from_slice(&large_id_page.body).unwrap();
    assert_eq!(
        large_id_page["entries"][0]["id"],
        exact_large_id.to_string()
    );
    assert!(large_id_page["entries"][0]["id"].is_string());

    let library = app.handle(&req(&get(
        "/api/web/library?view=library&kind=audio&q=fixture&offset=0&limit=1",
        "Browser/1.0",
    )));
    assert_eq!(library.status, 200);
    let value: serde_json::Value = serde_json::from_slice(&library.body).unwrap();
    assert_eq!(value["server_name"], "rustyDLNA-test");
    assert_eq!(value["capabilities"]["transcoding"], true);
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    assert_eq!(value["entries"][0]["entry_type"], "media");
    assert_eq!(value["entries"][0]["kind"], "audio");
    assert!(value["entries"][0]["title"].is_string());
    assert!(value["entries"][0]["file_name"]
        .as_str()
        .unwrap()
        .contains('.'));
    assert!(value["entries"][0]["duration_seconds"].is_number());
    assert!(value["entries"][0]["source_url"]
        .as_str()
        .unwrap()
        .starts_with("/web/media/"));
    assert!(value["entries"][0]["download_url"].is_null());
    let audio_item_id = value["entries"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let audio_download = app.handle(&req(&get(
        &format!("/web/download/{audio_item_id}"),
        "Browser/1.0",
    )));
    assert_eq!(audio_download.status, 415);
    assert!(resp_header(&audio_download, "Content-Disposition").is_none());
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&audio_item_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.audio = "aac".into();
        item.probe.audio_streams = "0:0:aac:2".into();
        item.duration = Some("00:01:00.000".into());
    }
    let audio_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=audio&q=fixture&offset=0&limit=1",
        "Browser/1.0",
    )));
    let audio_page: serde_json::Value = serde_json::from_slice(&audio_page.body).unwrap();
    assert_eq!(
        audio_page["entries"][0]["audio_tracks"][0]["content_type"],
        "audio/mp4; codecs=\"mp4a.40.2\""
    );
    let copied_audio = app.handle(&req(&get(
        &format!(
            "/web/media/{audio_item_id}.mp4?quality=auto&video_mode=transcode&audio_mode=copy"
        ),
        "Browser/1.0",
    )));
    assert_eq!(
        copied_audio.status,
        200,
        "{}",
        String::from_utf8_lossy(&copied_audio.body)
    );
    let copied_audio_spec = copied_audio.remux_job.expect("browser audio-only copy job");
    assert!(copied_audio_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:a", "copy"]));
    assert!(copied_audio_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-bsf:a", "aac_adtstoasc"]));
    assert!(copied_audio_spec
        .fallback_args
        .as_ref()
        .is_some_and(|args| args.windows(2).any(|pair| pair == ["-c:a", "aac"])));

    let hdr = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=dvp7&limit=10",
        "Browser/1.0",
    )));
    let hdr: serde_json::Value = serde_json::from_slice(&hdr.body).unwrap();
    assert!(hdr["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["transcode_likely"] == true));

    let caption_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=movie&limit=20",
        "Browser/1.0",
    )));
    assert_eq!(caption_page.status, 200);
    let caption_page_text = String::from_utf8_lossy(&caption_page.body);
    assert!(!caption_page_text.contains("testdata/library"));
    let caption_page: serde_json::Value = serde_json::from_slice(&caption_page.body).unwrap();
    let artwork_item = caption_page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["art_url"].is_string())
        .expect("fixture video with artwork");
    let artwork_url = artwork_item["art_url"].as_str().unwrap();
    assert!(artwork_url.starts_with("/AlbumArt/"), "{artwork_url}");
    assert!(artwork_url.ends_with(".jpg"), "{artwork_url}");
    let caption_item = caption_page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["captions"]
                .as_array()
                .is_some_and(|captions| !captions.is_empty())
        })
        .expect("captioned movie in fixture library");
    assert!(caption_item["captions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|caption| caption["language"] == "en"));
    let caption_url = caption_item["captions"][0]["url"].as_str().unwrap();
    let caption = app.handle(&req(&get(caption_url, "Browser/1.0")));
    assert_eq!(caption.status, 200);
    assert_eq!(
        resp_header(&caption, "Content-Type"),
        Some("text/vtt; charset=utf-8")
    );
    assert!(caption.body.starts_with(b"WEBVTT\n\n"));

    let h264_audio_fallback = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("tagged.mp4"))
        .cloned()
        .unwrap();
    let enriched_video = app.handle(&req(&get(
        &format!("/api/web/item/{}?enrich=1", h264_audio_fallback.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(
        enriched_video.status,
        200,
        "{}",
        String::from_utf8_lossy(&enriched_video.body)
    );
    let enriched_video: serde_json::Value = serde_json::from_slice(&enriched_video.body).unwrap();
    assert_eq!(enriched_video["item"]["stream_metadata_complete"], true);
    assert!(
        enriched_video["item"]["video_content_type"]
            .as_str()
            .is_some_and(|content_type| content_type.starts_with("video/mp4; codecs=\"avc1.")),
        "{enriched_video}"
    );
    assert!(enriched_video["item"]["video_profile"].is_string());
    assert!(enriched_video["item"]["pixel_format"].is_string());
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&h264_audio_fallback.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.video = "h264".into();
        item.probe.video_profile = "High".into();
        item.probe.video_level = 41;
        item.probe.pixel_format = "yuv420p".into();
        item.probe.bit_depth = 8;
        item.probe.codec_string = "avc1.640029,ac-3".into();
        item.probe.hdr = "sdr".into();
        item.probe.audio = "ac3".into();
        item.probe.audio_streams = "1:0:ac3:6".into();
        item.duration = Some("00:01:00.000".into());
    }
    let h264_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=tagged&limit=10",
        "Browser/1.0",
    )));
    let h264_page: serde_json::Value = serde_json::from_slice(&h264_page.body).unwrap();
    let h264_audio_fallback_id = h264_audio_fallback.detail_id.to_string();
    let h264_dto = h264_page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"].as_str() == Some(h264_audio_fallback_id.as_str()))
        .unwrap();
    assert_eq!(
        h264_dto["video_content_type"],
        "video/mp4; codecs=\"avc1.640029\""
    );
    assert_eq!(
        h264_dto["audio_tracks"][0]["content_type"],
        "audio/mp4; codecs=\"ac-3\""
    );
    assert_eq!(h264_dto["codec_string"], "avc1.640029,ac-3");
    assert_eq!(h264_dto["compatible_video_encoder"], "libx264");
    let audio_only_compat = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=transcode",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(
        audio_only_compat.status,
        200,
        "{}",
        String::from_utf8_lossy(&audio_only_compat.body)
    );
    let audio_only_spec = audio_only_compat
        .remux_job
        .expect("browser audio-only compatibility job");
    assert!(audio_only_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "copy"]));
    assert!(audio_only_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:a", "aac"]));
    assert!(audio_only_spec
        .fallback_args
        .as_ref()
        .is_some_and(|args| args.iter().any(|arg| arg == "libx264")));
    let aligned_audio_seek = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?start=30&quality=auto&video_mode=copy&audio_mode=transcode",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    let aligned_audio_seek = aligned_audio_seek
        .remux_job
        .expect("mixed copied-video/transcoded-audio seek job");
    assert!(aligned_audio_seek
        .args
        .windows(3)
        .any(|args| args == ["-noaccurate_seek", "-ss", "30"]));
    assert!(aligned_audio_seek
        .fallback_args
        .as_ref()
        .is_some_and(|args| !args.iter().any(|arg| arg == "-noaccurate_seek")));
    let resized_compat = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?start=30&quality=full_hd&video_mode=transcode&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    let resized_spec = resized_compat
        .remux_job
        .expect("explicit browser quality job");
    assert!(resized_spec.args.iter().any(|arg| arg == "libx264"));
    assert!(resized_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:a", "copy"]));
    assert!(!resized_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "copy"]));
    assert!(resized_spec
        .args
        .windows(2)
        .any(|args| args == ["-ss", "25"]));
    assert!(resized_spec
        .args
        .windows(2)
        .any(|args| args == ["-ss", "5"]));
    assert!(!resized_spec
        .args
        .iter()
        .any(|argument| argument == "-noaccurate_seek"));

    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&h264_audio_fallback.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.video = "hevc".into();
        item.probe.video_profile = "Main 10".into();
        item.probe.video_level = 120;
        item.probe.pixel_format = "yuv420p10le".into();
        item.probe.bit_depth = 10;
        item.probe.codec_string = "hev1.1.6.L120.B0,mp4a.40.2".into();
        item.probe.hdr = "dv-p8".into();
        item.probe.audio = "aac".into();
        item.probe.audio_streams = "1:0:aac:2".into();
    }
    let hevc_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=tagged&limit=10",
        "Browser/1.0",
    )));
    let hevc_page: serde_json::Value = serde_json::from_slice(&hevc_page.body).unwrap();
    assert_eq!(
        hevc_page["entries"][0]["video_content_type"],
        "video/mp4; codecs=\"hvc1.1.6.L120.B0\""
    );
    let copied_compat = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    let copied_spec = copied_compat
        .remux_job
        .expect("fully copied HEVC/AAC browser job");
    assert!(copied_spec.cache_key.contains("aligned-seek-v2"));
    let mut preset_keys = std::collections::HashSet::new();
    for (preset, expected) in [
        ("balanced", "veryfast"),
        ("fast_start", "veryfast"),
        ("maximum_speed", "ultrafast"),
    ] {
        let response = app.handle(&req(&get(
            &format!(
                "/web/media/{}.m3u8?delivery=hls&quality=full_hd&encoding_preset={preset}",
                h264_audio_fallback.detail_id,
            ),
            "Browser/encoding-presets",
        )));
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let spec = response.remux_job.unwrap();
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["-preset", expected]));
        assert_eq!(
            spec.args
                .windows(2)
                .any(|pair| pair == ["-tune", "zerolatency"]),
            preset != "balanced"
        );
        preset_keys.insert(spec.cache_key);
        let copied = app.handle(&req(&get(&format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=copy&encoding_preset={preset}", h264_audio_fallback.detail_id,
        ), "Browser/encoding-presets"))).remux_job.unwrap();
        assert_eq!(copied.cache_key, copied_spec.cache_key);
        assert_eq!(copied.args, copied_spec.args);
    }
    assert_eq!(preset_keys.len(), 3);
    for query in [
        "encoding_preset=unknown",
        "encoding_preset=",
        "encoding_preset=balanced&encoding_preset=fast_start",
    ] {
        assert_eq!(
            app.handle(&req(&get(
                &format!("/web/media/{}.mp4?{query}", h264_audio_fallback.detail_id,),
                "Browser/encoding-presets"
            )))
            .status,
            400
        );
    }
    for start in [0, 30] {
        let hls_copy = app.handle(&req(&get(
            &format!(
                "/web/media/{}.m3u8?delivery=hls&quality=auto&video_mode=copy&audio_mode=transcode&start={start}",
                h264_audio_fallback.detail_id
            ),
            "Safari/HEVC-HLS-trial",
        )));
        assert_eq!(
            hls_copy.status,
            200,
            "{}",
            String::from_utf8_lossy(&hls_copy.body)
        );
        let hls_copy = hls_copy.remux_job.expect("Safari HEVC HLS copy job");
        assert!(!hls_copy.hls_all_fragments_independent);
        for pair in [["-c:v", "copy"], ["-c:a", "aac"], ["-tag:v", "hvc1"]] {
            assert!(hls_copy.args.windows(2).any(|args| args == pair));
        }
        assert!(!hls_copy.args.iter().any(|arg| arg == "-force_key_frames"));
        assert!(!hls_copy
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().contains("libplacebo")));
    }
    assert!(copied_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "copy"]));
    assert!(copied_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-tag:v", "hvc1"]));
    assert!(copied_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:a", "copy"]));
    assert!(copied_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-bsf:a", "aac_adtstoasc"]));
    assert!(copied_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-avoid_negative_ts", "make_zero"]));
    let portable = copied_spec
        .fallback_args
        .as_ref()
        .expect("portable fallback for negotiated copies");
    assert!(portable.iter().any(|arg| arg == "libx264"));
    assert!(portable.windows(2).any(|pair| pair == ["-c:a", "aac"]));
    app.cfg.web.encoder = "h264_nvenc".into();
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&h264_audio_fallback.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.hdr = "hdr10".into();
        item.probe.video_timestamp_mode = "broken-reordered".into();
        item.probe.audio_streams.push_str(",@t:broken-reordered");
    }
    let repair_page = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&q=tagged&limit=10",
        "Browser/1.0",
    )));
    let repair_page: serde_json::Value = serde_json::from_slice(&repair_page.body).unwrap();
    assert_eq!(repair_page["entries"][0]["video_repair_required"], true);
    assert_eq!(
        repair_page["entries"][0]["repair_video_encoder"],
        "hevc_nvenc"
    );
    assert!(repair_page["entries"][0]["video_content_type"].is_string());
    let unsafe_copy = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(unsafe_copy.status, 400);
    let repaired = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=repair&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    let repaired_spec = repaired.remux_job.expect("HEVC frame-order repair job");
    assert!(repaired_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "hevc_nvenc"]));
    assert!(
        repaired_spec.args.iter().any(|arg| arg == "format=p010le")
            || repaired_spec
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("format=p010le"))
    );
    assert!(repaired_spec
        .args
        .windows(2)
        .any(|pair| pair == ["-tag:v", "hvc1"]));
    assert!(!repaired_spec
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("libplacebo")));
    assert!(repaired_spec.fallback_args.as_ref().is_some_and(|args| args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("libplacebo"))));
    app.cfg.web.encoder = "libx264".into();
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&h264_audio_fallback.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.codec_string = "hevc,mp4a.40.2".into();
        item.probe.hdr = "dv-p8".into();
        item.probe.video_timestamp_mode = "valid".into();
    }
    let legacy_metadata_copy = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(
        legacy_metadata_copy.status,
        200,
        "{}",
        String::from_utf8_lossy(&legacy_metadata_copy.body)
    );
    assert!(legacy_metadata_copy
        .remux_job
        .expect("legacy HEVC metadata copy job")
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "copy"]));
    let invalid_resized_copy = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=full_hd&video_mode=copy&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(invalid_resized_copy.status, 400);
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&h264_audio_fallback.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.audio = "truehd".into();
        item.probe.audio_streams = "1:0:truehd:8".into();
    }
    let invalid_audio_copy = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=auto&video_mode=copy&audio_mode=copy",
            h264_audio_fallback.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(invalid_audio_copy.status, 400);

    let dvp7 = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("dvp7.mkv"))
        .cloned()
        .unwrap();
    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&dvp7.detail_id].clone();
        catalog
            .items
            .get_mut(&object_id)
            .unwrap()
            .probe
            .audio_streams
            .push_str(",@c:1250:4000:Intro%3A %D0%9A%D0%B8%D0%BD%D0%BE");
    }
    let stream_details = app.handle(&req(&get(
        &format!("/api/web/item/{}", dvp7.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(stream_details.status, 200);
    assert!(resp_header(&stream_details, "ETag").is_some());
    let stream_details: serde_json::Value = serde_json::from_slice(&stream_details.body).unwrap();
    assert_eq!(stream_details["item"]["id"], dvp7.detail_id.to_string());
    assert_eq!(
        stream_details["item"]["download_url"],
        format!("/web/download/{}", dvp7.detail_id)
    );
    assert_eq!(stream_details["audio_tracks"][0]["codec"], "truehd");
    assert_eq!(stream_details["item"]["stream_metadata_complete"], true);
    assert_eq!(stream_details["chapters"][0]["title"], "Intro: Кино");
    assert_eq!(stream_details["chapters"][0]["start_seconds"], 1.25);
    assert_eq!(
        stream_details["item"]["chapters"],
        stream_details["chapters"]
    );
    let direct = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?mode=direct&reason=forced_original&request=8",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(direct.status, 200);
    assert!(direct.remux_job.is_none());
    assert!(direct.file_range.is_some() || !direct.body.is_empty());
    assert_eq!(resp_header(&direct, "Accept-Ranges"), Some("bytes"));
    let download = app.handle(&req(&format!(
        "GET /web/download/{} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\nRange: bytes=0-15\r\n\r\n",
        dvp7.detail_id
    )));
    assert_eq!(download.status, 206);
    assert_eq!(resp_header(&download, "Content-Length"), Some("16"));
    assert_eq!(
        resp_header(&download, "Content-Type"),
        Some(dvp7.mime.as_str())
    );
    assert_eq!(resp_header(&download, "Accept-Ranges"), Some("bytes"));
    assert_eq!(
        resp_header(&download, "Content-Range"),
        Some(format!("bytes 0-15/{}", dvp7.size).as_str())
    );
    assert_eq!(
        resp_header(&download, "Content-Disposition"),
        Some("attachment; filename=\"dvp7.mkv\"; filename*=UTF-8''dvp7.mkv")
    );
    assert_eq!(
        resp_header(&download, "Cache-Control"),
        Some("private, no-store")
    );
    assert_eq!(
        resp_header(&download, "X-Content-Type-Options"),
        Some("nosniff")
    );
    assert!(download.remux_job.is_none());
    assert_eq!(download.body.len(), 16);
    for invalid in [
        format!("/web/download/{}?extra=1", dvp7.detail_id),
        "/web/download/0".to_owned(),
        "/web/download/01".to_owned(),
        "/web/download/nope".to_owned(),
    ] {
        assert_eq!(
            app.handle(&req(&get(&invalid, "Browser/1.0"))).status,
            400,
            "{invalid}"
        );
    }
    let compat = app.handle(&req(&get(
        &format!("/web/media/{}.mp4", dvp7.detail_id),
        "Browser/1.0",
    )));
    let spec = compat.remux_job.expect("browser compatibility job");
    assert_eq!(spec.mime, "video/mp4");
    assert!(spec.verified_ffmpeg.is_some());
    assert!(!spec.continue_after_disconnect);
    assert!(spec.cacheable);
    assert!(spec.fallback_args.is_none());
    let args = spec
        .args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg == "libx264"), "{args:?}");
    assert!(args.iter().any(|arg| arg == "aac"), "{args:?}");
    assert!(args.iter().any(|arg| arg == "0:v:0?"), "{args:?}");
    assert!(
        args.windows(2)
            .any(|pair| pair == ["-init_hw_device", "vulkan=vk:0"]),
        "{args:?}"
    );
    assert!(
        args.iter()
            .any(|arg| arg.contains("libplacebo=apply_dolbyvision=true")),
        "{args:?}"
    );

    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&dvp7.detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.audio_streams = "1:0:truehd:6,2:1:ac3:2".into();
        item.duration = Some("1:00:00.000".into());
    }
    let selected = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?audio=1&start=120&session=12&request=77",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    let selected = selected.remux_job.expect("selected browser audio job");
    assert_eq!(selected.audio_index, 1);
    assert_eq!(selected.web_session_id, Some(12));
    assert_eq!(selected.web_request_id, Some(77));
    assert!(selected.cache_key.ends_with("-start-120"));
    assert!(!selected.cacheable);
    let selected_source = Arc::clone(
        selected
            .source_file
            .as_ref()
            .expect("browser seek retains its source descriptor"),
    );
    let selected_base_key = selected
        .cache_key
        .strip_suffix("-start-120")
        .unwrap()
        .to_owned();
    let selected_args = selected
        .args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(
        selected_args.windows(2).any(|pair| pair == ["-ss", "120"]),
        "{selected_args:?}"
    );
    assert!(!selected_args.iter().any(|arg| arg == "-noaccurate_seek"));
    assert!(
        selected_args.iter().any(|arg| arg == "0:a:1?"),
        "{selected_args:?}"
    );
    let later_seek = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?audio=1&start=240&session=12&request=78",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    let later_seek = later_seek
        .remux_job
        .expect("later browser seek reuses prepared identity");
    assert!(Arc::ptr_eq(
        &selected_source,
        later_seek.source_file.as_ref().unwrap()
    ));
    assert_eq!(
        later_seek.cache_key.strip_suffix("-start-240").unwrap(),
        selected_base_key
    );
    assert!(later_seek
        .args
        .windows(2)
        .any(|pair| pair == ["-ss", "240"]));
    assert_eq!(app.web_transcode_preparations.lock().unwrap().len(), 1);
    let changed_plan = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?audio=1&quality=data_saver&start=300&session=12&request=79",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    let changed_plan = changed_plan
        .remux_job
        .expect("changed quality builds a new prepared identity");
    assert!(!Arc::ptr_eq(
        &selected_source,
        changed_plan.source_file.as_ref().unwrap()
    ));
    assert!(changed_plan
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("min(iw,1280)")));
    assert_eq!(app.web_transcode_preparations.lock().unwrap().len(), 1);
    assert_eq!(
        app.handle(&req(&get(
            &format!("/web/media/{}.mp4?audio=2", dvp7.detail_id),
            "Browser/1.0",
        )))
        .status,
        400
    );
    let saver = app.handle(&req(&get(
        &format!("/web/media/{}.mp4?quality=data_saver", dvp7.detail_id),
        "Browser/1.0",
    )));
    let saver = saver.remux_job.expect("data saver browser job");
    assert!(saver.args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("libplacebo=apply_dolbyvision=true")));
    assert!(saver.args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("w='min(iw,1280)':h='min(ih,720)'")));
    assert!(!saver
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains(",scale=")));
    assert!(saver
        .args
        .windows(2)
        .any(|pair| pair == ["-maxrate", "3000k"]));
    let full_hd = app.handle(&req(&get(
        &format!("/web/media/{}.mp4?quality=full_hd", dvp7.detail_id),
        "Browser/1.0",
    )));
    let full_hd = full_hd.remux_job.expect("1080p browser job");
    assert!(full_hd.args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("w='min(iw,1920)':h='min(ih,1080)'")));
    assert!(full_hd
        .args
        .windows(2)
        .any(|pair| pair == ["-maxrate", "8000k"]));
    let hls = app.handle(&req(&get(
        &format!(
            "/web/media/{}.m3u8?quality=data_saver&delivery=hls",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    let hls = hls.remux_job.expect("HLS must use the bounded browser job");
    assert!(hls.hls_all_fragments_independent);
    let mse = app.handle(&req(&get(
        &format!(
            "/web/media/{}.m3u8?quality=data_saver&delivery=mse",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    let mse = mse
        .remux_job
        .expect("Media Source fragments must use the bounded browser job");
    assert!(mse.hls_all_fragments_independent);
    let mse_delta = app.handle(&req(&get(
        &format!(
            "/web/media/{}.m3u8?quality=data_saver&delivery=mse&mse_after=17",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    assert!(
        mse_delta.remux_job.is_some(),
        "Media Source delta playlist must use the bounded browser job"
    );
    for resource in [
        format!(
            "/web/media/{}.mp4?quality=data_saver&delivery=hls_init&hls_offset=0&hls_length=1024",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m4s?quality=data_saver&delivery=hls_segment&hls_offset=1024&hls_length=4096",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.mp4?quality=data_saver&delivery=mse_init&hls_offset=0&hls_length=1024",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m4s?quality=data_saver&delivery=mse_segment&hls_offset=1024&hls_length=4096",
            dvp7.detail_id
        ),
    ] {
        assert!(
            app.handle(&req(&get(&resource, "Browser/1.0")))
                .remux_job
                .is_some(),
            "fixed HLS resource must attach to the browser job: {resource}"
        );
    }
    assert_eq!(
        app.handle(&req(&get(
            &format!("/web/media/{}.mp4?quality=ultra", dvp7.detail_id),
            "Browser/1.0",
        )))
        .status,
        400
    );
    let transcode_status = app.handle(&req(&get(
        &format!("/api/web/transcode/{}", dvp7.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(transcode_status.status, 200);
    let transcode_status: serde_json::Value =
        serde_json::from_slice(&transcode_status.body).unwrap();
    assert_eq!(transcode_status["state"], "idle");
    let scoped_status = app.handle(&req(&get(
        &format!("/api/web/transcode/{}?request=77", dvp7.detail_id),
        "Browser/1.0",
    )));
    let scoped_status: serde_json::Value = serde_json::from_slice(&scoped_status.body).unwrap();
    assert_eq!(scoped_status["request_id"], 77);
    let cancel_status = app.handle(&req(&format!(
        "DELETE /api/web/transcode/{}?request=77&session=12 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\n\r\n",
        dvp7.detail_id
    )));
    assert_eq!(cancel_status.status, 200);
    let cancel_status: serde_json::Value = serde_json::from_slice(&cancel_status.body).unwrap();
    assert_eq!(cancel_status["request_id"], 77);
    assert_eq!(cancel_status["state"], "cancelled");
    let canplay_status = app.handle(&req(&format!(
        "POST /api/web/transcode/{}?request=78&session=12&event=canplay HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\nContent-Length: 0\r\n\r\n",
        dvp7.detail_id
    )));
    assert_eq!(
        canplay_status.status,
        200,
        "{}",
        String::from_utf8_lossy(&canplay_status.body)
    );
    assert_eq!(
        app.handle(&req(&format!(
            "POST /api/web/transcode/{}?event=canplay HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nContent-Length: 0\r\n\r\n",
            dvp7.detail_id
        )))
        .status,
        400
    );
    assert_eq!(
        app.handle(&req(&format!(
            "POST /api/web/transcode/{}?request=78&session=12&event=mse_first_fragment_appended HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nContent-Length: 0\r\n\r\n",
            dvp7.detail_id
        )))
        .status,
        200
    );
    assert_eq!(
        app.handle(&req(&format!(
            "POST /api/web/transcode/{}?request=78&session=12&event=nope HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nContent-Length: 0\r\n\r\n",
            dvp7.detail_id
        )))
        .status,
        400
    );
    assert_eq!(
        app.handle(&req(&get(
            &format!(
                "/api/web/transcode/{}?request=78&session=12&event=canplay",
                dvp7.detail_id
            ),
            "Browser/1.0",
        )))
        .status,
        400
    );
    let unscoped_cancel = app.handle(&req(&format!(
        "DELETE /api/web/transcode/{} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\n\r\n",
        dvp7.detail_id
    )));
    assert_eq!(unscoped_cancel.status, 400);
    for invalid in [
        format!("/web/media/{}.mp4?mode=maybe", dvp7.detail_id),
        format!("/web/media/{}.mp4?request=-1", dvp7.detail_id),
        format!("/web/media/{}.mp4?session=-1", dvp7.detail_id),
        format!(
            "/web/media/{}.mp4?mode=direct&mode=compatible",
            dvp7.detail_id
        ),
        format!("/web/media/{}.mp4?mode=direct&quality=auto", dvp7.detail_id),
        format!("/web/media/{}.mp4?delivery=maybe", dvp7.detail_id),
        format!("/web/media/{}.mp4?mode=direct&delivery=hls", dvp7.detail_id),
        format!("/web/media/{}.mp4?delivery=hls", dvp7.detail_id),
        format!("/web/media/{}.m3u8?delivery=hls_init", dvp7.detail_id),
        format!("/web/media/{}.m4s?delivery=hls_segment", dvp7.detail_id),
        format!("/web/media/{}.mp4?delivery=mse", dvp7.detail_id),
        format!("/web/media/{}.m3u8?delivery=mse_init", dvp7.detail_id),
        format!("/web/media/{}.m4s?delivery=mse_segment", dvp7.detail_id),
        format!(
            "/web/media/{}.m3u8?delivery=hls&mse_after=1",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.mp4?delivery=mse_init&hls_offset=0&hls_length=1024&mse_after=1",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m3u8?delivery=mse&mse_after=20001",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m3u8?delivery=mse&mse_after=nope",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m3u8?%64elivery=mse&mse_after=1",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m3u8?delivery=mse&%6dse_after=1",
            dvp7.detail_id
        ),
        format!(
            "/web/media/{}.m4s?delivery=hls_segment&hls_offset=0&hls_length=0",
            dvp7.detail_id
        ),
        format!("/web/media/{}.mp4?reason=", dvp7.detail_id),
        format!("/web/media/{}.mp4?reason=%GG", dvp7.detail_id),
        format!("/web/media/{}.mp4?video_output=unknown", dvp7.detail_id),
        format!(
            "/web/media/{}.mp4?video_mode=copy&video_output=h264_sdr",
            dvp7.detail_id
        ),
        format!("/web/media/{}.mp4?video_output=hevc_hdr10", dvp7.detail_id),
        format!("/api/web/transcode/{}?request=nope", dvp7.detail_id),
        format!("/api/web/transcode/{}?session=nope", dvp7.detail_id),
        format!("/api/web/transcode/{}?extra=1", dvp7.detail_id),
        format!("/api/web/item/{}?enrich=maybe", dvp7.detail_id),
        format!("/api/web/item/{}?extra=1", dvp7.detail_id),
    ] {
        let response = app.handle(&req(&get(&invalid, "Browser/1.0")));
        assert_eq!(response.status, 400, "{invalid}");
        let error: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error["schema_version"], 2);
        assert!(error["error"]["code"].is_string());
    }

    app.cfg.web.encoder = "h264_nvenc".into();
    let accelerated_capabilities = app.handle(&req(&get(
        "/api/web/library?view=library&kind=video&limit=1",
        "Browser/1.0",
    )));
    let accelerated_capabilities: serde_json::Value =
        serde_json::from_slice(&accelerated_capabilities.body).unwrap();
    let accelerated_outputs = accelerated_capabilities["capabilities"]["video_outputs"]
        .as_array()
        .unwrap();
    assert_eq!(accelerated_outputs.len(), 2);
    assert_eq!(accelerated_outputs[1]["id"], "hevc_hdr10");
    assert_eq!(accelerated_outputs[1]["dynamic_range"], "hdr");

    let hdr10_output = app.handle(&req(&get(
        &format!(
            "/web/media/{}.mp4?quality=uhd_high&video_mode=transcode&video_output=hevc_hdr10&audio_mode=transcode",
            dvp7.detail_id
        ),
        "Browser/1.0",
    )));
    assert_eq!(
        hdr10_output.status,
        200,
        "{}",
        String::from_utf8_lossy(&hdr10_output.body)
    );
    let hdr10_output = hdr10_output.remux_job.expect("HDR10 browser job");
    assert!(hdr10_output
        .args
        .windows(2)
        .any(|pair| pair == ["-c:v", "hevc_nvenc"]));
    assert!(hdr10_output
        .args
        .windows(2)
        .any(|pair| pair == ["-profile:v", "main10"]));
    assert!(hdr10_output
        .args
        .windows(2)
        .any(|pair| pair == ["-maxrate", "25000k"]));
    assert!(hdr10_output
        .args
        .windows(2)
        .any(|pair| pair == ["-color_trc", "smpte2084"]));
    assert!(!hdr10_output
        .args
        .iter()
        .any(|argument| argument == "-hwaccel"));
    assert!(!hdr10_output
        .args
        .iter()
        .any(|argument| argument.to_string_lossy().contains("libplacebo")));

    let accelerated = app.handle(&req(&get(
        &format!("/web/media/{}.mp4?start=60", dvp7.detail_id),
        "Browser/1.0",
    )));
    let accelerated = accelerated.remux_job.expect("NVENC browser job");
    assert!(accelerated.args.iter().any(|arg| arg == "h264_nvenc"));
    assert!(accelerated
        .args
        .windows(2)
        .any(|pair| pair == ["-hwaccel", "cuda"]));
    assert!(accelerated
        .args
        .windows(2)
        .any(|pair| pair == ["-init_hw_device", "vulkan=vk:0"]));
    assert!(accelerated
        .args
        .windows(2)
        .any(|pair| pair == ["-filter_hw_device", "vk"]));
    assert!(accelerated.args.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg.contains("hwdownload,format=p010le,hwupload")
            && arg.contains("libplacebo=apply_dolbyvision=true")
            && arg.contains("colorspace=bt709")
    }));
    assert!(accelerated
        .args
        .windows(2)
        .any(|pair| pair == ["-color_trc", "bt709"]));
    let fallback = accelerated
        .fallback_args
        .as_ref()
        .expect("NVENC software fallback");
    assert!(fallback.iter().any(|arg| arg == "libx264"));
    assert!(!fallback.iter().any(|arg| arg == "-hwaccel"));
    assert!(fallback.windows(2).any(|pair| pair == ["-ss", "60"]));
    assert!(fallback.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg.contains("format=yuv420p10le,hwupload")
            && arg.contains("libplacebo=apply_dolbyvision=true")
    }));

    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&dvp7.detail_id].clone();
        catalog.items.get_mut(&object_id).unwrap().probe.hdr = "sdr".into();
    }
    let accelerated_sdr = app.handle(&req(&get(
        &format!("/web/media/{}.mp4", dvp7.detail_id),
        "Browser/1.0",
    )));
    let accelerated_sdr = accelerated_sdr.remux_job.expect("NVENC SDR browser job");
    assert!(accelerated_sdr.args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("scale_cuda=w='min(iw,3840)':h='min(ih,2160)'")));
    assert!(!accelerated_sdr
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("libplacebo")));

    {
        let mut catalog = app.catalog.write().unwrap();
        let object_id = catalog.by_detail[&dvp7.detail_id].clone();
        catalog.items.get_mut(&object_id).unwrap().probe.video = "h264".into();
    }
    let accelerated_h264 = app.handle(&req(&get(
        &format!("/web/media/{}.mp4?quality=data_saver", dvp7.detail_id),
        "Browser/1.0",
    )));
    let accelerated_h264 = accelerated_h264.remux_job.expect("NVENC H.264 browser job");
    assert!(!accelerated_h264
        .args
        .iter()
        .any(|argument| argument == "-hwaccel"));
    assert!(accelerated_h264.args.iter().any(|argument| argument
        .to_string_lossy()
        .contains("scale=w='min(iw,1280)':h='min(ih,720)'")));

    let runtime = app.handle(&req(&get("/api/status", "Browser/1.0")));
    let runtime: serde_json::Value = serde_json::from_slice(&runtime.body).unwrap();
    assert!(runtime["transcode"]["web_player"]["requests_total"].is_number());
    assert!(runtime["transcode"]["web_player"]["prepared_reuses_total"].is_number());
    for phase in [
        "startup_to_initial_bytes_ms",
        "startup_to_first_playable_ms",
        "startup_to_playlist_ready_ms",
        "startup_to_mse_playlist_received_ms",
        "startup_to_mse_init_fetched_ms",
        "startup_to_mse_init_appended_ms",
        "startup_to_mse_first_fragment_fetched_ms",
        "startup_to_mse_first_fragment_appended_ms",
        "startup_to_canplay_ms",
        "startup_to_playing_ms",
    ] {
        assert!(runtime["transcode"]["web_player"][phase]["count"].is_number());
    }

    app.cfg.transcode.enable = false;
    let disabled_compatible = app.handle(&req(&get(
        &format!("/web/media/{}.mp4?mode=compatible", dvp7.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(disabled_compatible.status, 409);
    let disabled_compatible: serde_json::Value =
        serde_json::from_slice(&disabled_compatible.body).unwrap();
    assert_eq!(disabled_compatible["error"]["code"], "transcode_disabled");

    app.cfg.web.enable = false;
    let disabled_root = app.handle(&req(&get("/", "Browser/1.0")));
    let status = app.handle(&req(&get("/status", "Browser/1.0")));
    assert_eq!(disabled_root.body, status.body);
    assert_eq!(
        app.handle(&req(&get("/api/web/library", "Browser/1.0")))
            .status,
        404
    );
    assert_eq!(
        app.handle(&req(&get("/web/app.js", "Browser/1.0"))).status,
        404
    );
    assert_eq!(
        app.handle(&req(&get(
            &format!("/api/web/item/{}", dvp7.detail_id),
            "Browser/1.0",
        )))
        .status,
        404
    );
    assert_eq!(
        app.handle(&req(&get(
            &format!("/web/download/{}", dvp7.detail_id),
            "Browser/1.0",
        )))
        .status,
        404
    );
}

#[test]
fn web_item_does_not_advertise_aac_as_mpeg4_part_2_video_support() {
    let app = testdata_app();
    let detail_id = {
        let mut catalog = app.catalog.write().unwrap();
        let detail_id = catalog
            .items
            .values()
            .find(|item| item.path.ends_with("tagged.mp4"))
            .unwrap()
            .detail_id;
        let object_id = catalog.by_detail[&detail_id].clone();
        let item = catalog.items.get_mut(&object_id).unwrap();
        item.probe.container = "mp4".into();
        item.probe.video = "mpeg4".into();
        item.probe.codec_string = "mp4a.40.2".into();
        item.probe.audio = "aac".into();
        item.probe.audio_streams = "0:0:aac:2".into();
        detail_id
    };

    let response = app.handle(&req(&get(
        &format!("/api/web/item/{detail_id}"),
        "Browser/1.0",
    )));
    assert_eq!(response.status, 200);
    let payload: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(payload["item"]["video_codec"], "mpeg4");
    assert_eq!(payload["item"]["audio_codec"], "aac");
    assert_eq!(payload["item"]["codec_string"], serde_json::Value::Null);
    assert_eq!(
        payload["item"]["video_content_type"],
        serde_json::Value::Null
    );
    assert_eq!(payload["item"]["transcode_likely"], true);
}

#[test]
fn web_timeline_previews_are_complete_confined_and_source_bound() {
    use std::os::unix::fs::MetadataExt;

    let test_tree = TestTree::new("web-trickplay");
    let root = test_tree.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("Preview Movie.mkv");
    rusty_dlna_scan::write_fake_mkv(&source, 64);
    let cfg = Config {
        friendly_name: "preview-test".into(),
        media_dir: vec![root.display().to_string()],
        cache_dir: Some(test_tree.path().join("cache").display().to_string()),
        db_dir: Some(test_tree.path().join("database").display().to_string()),
        thumbnails: false,
        rescan_secs: 0,
        ..Config::default()
    };
    let app = App::from_config(cfg, 18200, 11900, test_tree.path());
    *write_recover(&app.catalog) = scan(&app.scan_cfg).unwrap();
    let item = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.path == source)
        .cloned()
        .expect("preview fixture item");
    let metadata = std::fs::metadata(&source).unwrap();
    let source_mtime_ns = metadata
        .mtime()
        .checked_mul(1_000_000_000)
        .unwrap()
        .checked_add(metadata.mtime_nsec())
        .unwrap();
    let directory = rusty_dlna_protocol::trickplay_directory_for_media(&source).unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    let revision = "0123456789abcdef";
    let jpeg = jpeg_declaring_dimensions(2880, 3780);
    for index in 0..58 {
        std::fs::write(
            directory.join(format!("sheet-{revision}-{index:04}.jpg")),
            &jpeg,
        )
        .unwrap();
    }
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "source_size": metadata.len(),
            "source_mtime_ns": source_mtime_ns,
            "duration_seconds": 1200.0,
            "interval_seconds": 1,
            "frame_width": 960,
            "frame_height": 540,
            "columns": 3,
            "rows": 7,
            "frame_count": 1200,
            "asset_revision": revision,
            "scale_divisor": 4,
        }))
        .unwrap(),
    )
    .unwrap();

    let item_response = app.handle(&req(&get(
        &format!("/api/web/item/{}", item.detail_id),
        "Browser/1.0",
    )));
    let item_json: serde_json::Value = serde_json::from_slice(&item_response.body).unwrap();
    assert_eq!(
        item_json["item"]["preview_url"],
        format!("/api/web/preview/{}", item.detail_id)
    );

    let manifest_response = app.handle(&req(&get(
        &format!("/api/web/preview/{}", item.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(manifest_response.status, 200);
    assert_eq!(
        resp_header(&manifest_response, "Cache-Control"),
        Some("private, max-age=0, must-revalidate")
    );
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_response.body).unwrap();
    assert_eq!(manifest["available"], true);
    assert_eq!(manifest["interval_seconds"], 1);
    assert_eq!(manifest["sheet_urls"].as_array().unwrap().len(), 58);
    let first_sheet = manifest["sheet_urls"][0].as_str().unwrap();
    let sheet_response = app.handle(&req(&get(first_sheet, "Browser/1.0")));
    assert_eq!(sheet_response.status, 200);
    assert_eq!(
        resp_header(&sheet_response, "Content-Type"),
        Some("image/jpeg")
    );
    assert_eq!(
        resp_header(&sheet_response, "Cache-Control"),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(sheet_response.body, jpeg);

    let final_sheet = directory.join(format!("sheet-{revision}-0057.jpg"));
    std::fs::remove_file(&final_sheet).unwrap();
    let incomplete = app.handle(&req(&get(
        &format!("/api/web/preview/{}", item.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(incomplete.status, 200);
    let incomplete: serde_json::Value = serde_json::from_slice(&incomplete.body).unwrap();
    assert_eq!(incomplete["available"], false);
    std::fs::write(final_sheet, &jpeg).unwrap();

    let portrait_revision = "fedcba9876543210";
    let portrait_jpeg = jpeg_declaring_dimensions(2880, 3412);
    for index in 0..240 {
        std::fs::write(
            directory.join(format!("sheet-{portrait_revision}-{index:04}.jpg")),
            &portrait_jpeg,
        )
        .unwrap();
    }
    let portrait_manifest = |interval_seconds, frame_count| {
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "source_size": metadata.len(),
            "source_mtime_ns": source_mtime_ns,
            "duration_seconds": 7200.0,
            "interval_seconds": interval_seconds,
            "frame_width": 960,
            "frame_height": 1706,
            "columns": 3,
            "rows": 2,
            "frame_count": frame_count,
            "asset_revision": portrait_revision,
        }))
        .unwrap()
    };
    std::fs::write(directory.join("manifest.json"), portrait_manifest(5, 1440)).unwrap();
    let portrait = app.handle(&req(&get(
        &format!("/api/web/preview/{}", item.detail_id),
        "Browser/1.0",
    )));
    let portrait: serde_json::Value = serde_json::from_slice(&portrait.body).unwrap();
    assert_eq!(portrait["available"], true);
    assert_eq!(portrait["interval_seconds"], 5);
    assert_eq!(portrait["sheet_urls"].as_array().unwrap().len(), 240);

    std::fs::write(directory.join("manifest.json"), portrait_manifest(6, 1200)).unwrap();
    let noncanonical_interval = app.handle(&req(&get(
        &format!("/api/web/preview/{}", item.detail_id),
        "Browser/1.0",
    )));
    let noncanonical_interval: serde_json::Value =
        serde_json::from_slice(&noncanonical_interval.body).unwrap();
    assert_eq!(noncanonical_interval["available"], false);

    std::fs::write(&source, b"source replacement changes its size").unwrap();
    let stale = app.handle(&req(&get(
        &format!("/api/web/preview/{}", item.detail_id),
        "Browser/1.0",
    )));
    assert_eq!(stale.status, 200);
    let stale: serde_json::Value = serde_json::from_slice(&stale.body).unwrap();
    assert_eq!(stale["available"], false);
}

#[test]
fn web_continue_watching_hydrates_only_bounded_requested_ids() {
    let app = testdata_app();
    let ids = {
        let catalog = app.catalog.read().unwrap();
        catalog
            .by_detail
            .keys()
            .copied()
            .filter(|id| {
                catalog.get_item_by_detail(*id).is_some_and(|item| {
                    item.mime.starts_with("video/") || item.mime.starts_with("audio/")
                })
            })
            .take(2)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids.len(), 2);

    let continue_path = format!("/api/web/library?view=continue&ids={},{}", ids[1], ids[0]);
    let response = app.handle(&req(&get(&continue_path, "Browser/1.0")));
    assert_eq!(response.status, 200);
    let etag = resp_header(&response, "ETag").unwrap().to_owned();
    let payload: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(payload["view"], "continue");
    assert_eq!(payload["library_state"], "ready");
    assert_eq!(payload["has_more"], false);
    assert_eq!(payload["total"], 2);
    assert_eq!(payload["entries"][0]["id"], ids[1].to_string());
    assert_eq!(payload["entries"][1]["id"], ids[0].to_string());
    assert_eq!(payload["entries"][0]["entry_type"], "media");
    let conditional = app.handle(&req(&format!(
        "GET {continue_path} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Browser/1.0\r\nIf-None-Match: {etag}\r\n\r\n"
    )));
    assert_eq!(conditional.status, 304);
    assert!(conditional.body.is_empty());

    let empty = app.handle(&req(&get(
        "/api/web/library?view=continue&ids=",
        "Browser/1.0",
    )));
    assert_eq!(empty.status, 200);
    let empty: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["library_state"], "ready");
    let missing = app.handle(&req(&get(
        "/api/web/library?view=continue&ids=9223372036854775807",
        "Browser/1.0",
    )));
    assert_eq!(missing.status, 200);
    let missing: serde_json::Value = serde_json::from_slice(&missing.body).unwrap();
    assert_eq!(missing["total"], 0);

    let stale_generation = app.update_id.load(Ordering::Acquire).saturating_add(1);
    assert_eq!(
        app.handle(&req(&get(
            &format!(
                "/api/web/library?view=continue&ids={}&generation={stale_generation}",
                ids[0]
            ),
            "Browser/1.0",
        )))
        .status,
        409
    );
    let too_many = (1..=101)
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    for invalid in [
        "/api/web/library?view=continue&ids=1,1".to_owned(),
        "/api/web/library?view=continue&ids=not-an-id".to_owned(),
        "/api/web/library?view=continue&ids=1&q=ignored".to_owned(),
        format!("/api/web/library?view=continue&ids={too_many}"),
    ] {
        assert_eq!(
            app.handle(&req(&get(&invalid, "Browser/1.0"))).status,
            400,
            "{invalid}"
        );
    }

    *app.catalog.write().unwrap() = Catalog::new();
    let generation = app.update_id.load(Ordering::Acquire);
    let pinned_empty = app.handle(&req(&get(
        &format!("/api/web/library?view=continue&ids=&generation={generation}"),
        "Browser/1.0",
    )));
    assert_eq!(pinned_empty.status, 200);
    let pinned_empty: serde_json::Value = serde_json::from_slice(&pinned_empty.body).unwrap();
    assert_eq!(pinned_empty["library_state"], "empty");
}

#[test]
fn web_repair_policy_keeps_hdr10_p8_and_tonemaps_dv5_p7() {
    let mut app = testdata_app();
    app.cfg.web.encoder = "h264_nvenc".into();
    let item = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.path.ends_with("tagged.mp4"))
        .cloned()
        .unwrap();

    for (hdr, expected_encoder, preserve_hdr) in [
        ("dv-p5", "h264_nvenc", false),
        ("dv-p7", "h264_nvenc", false),
        ("hdr10", "hevc_nvenc", true),
        ("dv-p8", "hevc_nvenc", true),
    ] {
        {
            let mut catalog = app.catalog.write().unwrap();
            let object_id = catalog.by_detail[&item.detail_id].clone();
            let item = catalog.items.get_mut(&object_id).unwrap();
            item.probe.video = "hevc".into();
            item.probe.video_profile = "Main 10".into();
            item.probe.pixel_format = "yuv420p10le".into();
            item.probe.bit_depth = 10;
            item.probe.hdr = hdr.into();
            item.probe.audio = "aac".into();
            item.probe.audio_streams = "1:0:aac:2,@t:broken-reordered".into();
            item.probe.video_timestamp_mode = "broken-reordered".into();
        }

        let page = app.handle(&req(&get(
            "/api/web/library?view=library&kind=video&q=tagged&limit=10",
            "Browser/1.0",
        )));
        assert_eq!(page.status, 200, "DTO for {hdr}");
        let page: serde_json::Value = serde_json::from_slice(&page.body).unwrap();
        let item_id = item.detail_id.to_string();
        let dto = page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"].as_str() == Some(item_id.as_str()))
            .unwrap();
        assert_eq!(dto["video_repair_required"], true, "DTO for {hdr}");
        assert_eq!(
            dto["repair_video_encoder"], expected_encoder,
            "DTO for {hdr}"
        );

        let response = app.handle(&req(&get(
            &format!(
                "/web/media/{}.mp4?quality=auto&video_mode=repair&audio_mode=copy",
                item.detail_id
            ),
            "Browser/1.0",
        )));
        assert_eq!(
            response.status,
            200,
            "route for {hdr}: {}",
            String::from_utf8_lossy(&response.body)
        );
        let spec = response.remux_job.expect("browser repair job");
        assert!(spec.verified_ffmpeg.is_some());
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair == ["-c:v", expected_encoder]),
            "argv for {hdr}: {:?}",
            spec.args
        );
        let has_sdr_filter = spec
            .args
            .iter()
            .any(|argument| argument.to_string_lossy().contains("libplacebo"));
        assert_eq!(has_sdr_filter, !preserve_hdr, "argv for {hdr}");
        assert!(
            spec.fallback_args.as_ref().is_some_and(|args| args
                .iter()
                .any(|argument| argument.to_string_lossy().contains("libplacebo"))),
            "software fallback must tone-map {hdr}"
        );
        assert_eq!(
            spec.cache_key.contains("sdr-tonemap-libplacebo-v2"),
            !preserve_hdr,
            "cache identity for {hdr}: {}",
            spec.cache_key
        );
        if preserve_hdr {
            assert!(
                spec.args
                    .windows(2)
                    .any(|pair| pair == ["-color_trc", "smpte2084"]),
                "HDR signaling for {hdr}: {:?}",
                spec.args
            );
        } else {
            assert!(
                spec.args
                    .windows(2)
                    .any(|pair| pair == ["-color_trc", "bt709"]),
                "SDR signaling for {hdr}: {:?}",
                spec.args
            );
        }
    }
}

#[test]
fn machine_status_is_structured_and_does_not_expose_absolute_paths() {
    let app = testdata_app();
    let response = app.handle(&req(&get("/api/status", "Operator/1.0")));
    assert_eq!(response.status, 200);
    assert_eq!(
        resp_header(&response, "Content-Type"),
        Some("application/json")
    );
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert!(matches!(
        value["status"].as_str(),
        Some("healthy" | "degraded")
    ));
    assert!(value["database"]["quick_check"].is_string());
    assert!(value["database"]["age_seconds"].is_u64());
    assert!(value["database"]["runs_total"].is_u64());
    assert!(value["database"]["pool"]["readers_total"].is_u64());
    assert!(value["listener"]["http"].is_string());
    assert!(value["transcode"]["cache_bytes"].is_u64());
    assert!(value["transcode"]["cache_hits_total"].is_u64());
    assert!(value["transcode"]["required_tools_ready"].is_boolean());
    assert!(value["helpers"]["active"].is_u64());
    assert!(value["helpers"]["queued"].is_u64());
    assert!(value["helpers"]["rejected_total"].is_u64());
    assert!(value["helpers"]["timed_out_total"].is_u64());
    assert!(value["helpers"]["saturated_total"].is_u64());
    assert!(value["helpers"]["wait_duration_ms"]["buckets"].is_array());
    assert!(value["scanner"]["files_seen"].is_u64());
    assert!(value["scanner"]["pending_paths"].is_u64());
    assert!(value["scanner"]["dropped_events_total"].is_u64());
    assert!(value["events"]["workers_alive"].is_u64());
    assert!(value["metrics"]["http"]["routes"].is_object());
    assert!(value["metrics"]["soap"]["browse_latency_ms"]["buckets"].is_array());
    assert!(value["catalog"]["estimated_memory_bytes"].is_u64());
    let catalog = &value["catalog"];
    for field in [
        "audio_records",
        "video_records",
        "image_records",
        "physical_inodes",
        "path_aliases",
        "media_records",
        "item_objects",
        "container_objects",
        "total_objects",
    ] {
        assert!(catalog[field].is_u64(), "missing catalog count {field}");
    }
    assert_eq!(
        catalog["media_records"].as_u64().unwrap(),
        catalog["physical_inodes"].as_u64().unwrap() + catalog["path_aliases"].as_u64().unwrap()
    );
    assert_eq!(
        catalog["total_objects"].as_u64().unwrap(),
        catalog["item_objects"].as_u64().unwrap() + catalog["container_objects"].as_u64().unwrap()
    );
    assert!(value["catalog"]["projected_memory_bytes"]["objects_10000"].is_u64());
    assert!(value["catalog"]["projected_memory_bytes"]["objects_100000"].is_u64());
    assert!(value["catalog"]["projected_memory_bytes"]["objects_1000000"].is_u64());
    let body = String::from_utf8(response.body).unwrap();
    assert!(
        !body.contains("/home/"),
        "status leaked an absolute path: {body}"
    );

    let health = app.handle(&req(&get("/health", "Operator/1.0")));
    assert!(matches!(health.status, 200 | 503));
    serde_json::from_slice::<serde_json::Value>(&health.body).unwrap();
}

#[test]
fn health_uses_cached_integrity_and_never_walks_the_catalog() {
    let app = testdata_app();
    let before = app.db_integrity.get(app.db_pool.clone()).runs_total;
    let catalog_guard = app.catalog.write().unwrap();
    for _ in 0..32 {
        let response = app.handle(&req(&get("/health", "Operator/1.0")));
        assert_eq!(response.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["database"]["runs_total"], before);
        assert!(value.get("catalog").is_none());
    }
    drop(catalog_guard);
    assert_eq!(app.db_integrity.get(app.db_pool.clone()).runs_total, before);

    {
        let mut snapshot = app.db_integrity.snapshot.lock().unwrap();
        snapshot.checked_unix = unix_now().saturating_sub(DB_CHECK_REFRESH_SECS);
    }
    for _ in 0..32 {
        let _ = app.handle(&req(&get("/health", "Operator/1.0")));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = app.db_integrity.get(app.db_pool.clone());
        if snapshot.runs_total == before + 1 && !snapshot.refresh_in_flight {
            break;
        }
        assert!(Instant::now() < deadline, "database refresh did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        app.db_integrity.get(app.db_pool.clone()).runs_total,
        before + 1
    );

    {
        let mut snapshot = app.db_integrity.snapshot.lock().unwrap();
        snapshot.result = DbIntegrityResult::Failed;
        snapshot.checked_unix = unix_now();
    }
    let failed = app.handle(&req(&get("/health", "Operator/1.0")));
    assert_eq!(failed.status, 503);
    let value: serde_json::Value = serde_json::from_slice(&failed.body).unwrap();
    assert_eq!(value["database"]["quick_check"], "failed");
}

#[test]
fn route_and_soap_metrics_are_fixed_cardinality_and_count_faults() {
    let app = testdata_app();
    assert_eq!(
        app.handle(&req(&get("/missing", "Metrics/1.0"))).status,
        404
    );
    let (ok, _) = soap_browse(&app, "0", "BrowseDirectChildren", "Metrics/1.0");
    assert_eq!(ok, 200);
    let (fault, _) = soap_browse(&app, "missing", "BrowseMetadata", "Metrics/1.0");
    assert_eq!(fault, 500);

    let response = app.handle(&req(&get("/api/status", "Metrics/1.0")));
    assert_eq!(response.status, 200);
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    let metrics = &value["metrics"];
    assert_eq!(
        metrics["http"]["routes"]["not_found"]["responses"]["4xx"],
        1
    );
    assert_eq!(metrics["soap"]["actions_total"]["Browse"], 2);
    assert_eq!(metrics["soap"]["faults_total"]["701"], 1);
    assert_eq!(metrics["soap"]["browse_latency_ms"]["count"], 2);
    assert_eq!(
        metrics["soap"]["browse_latency_ms"]["buckets"][10]["count"],
        2
    );
    assert!(!metrics.to_string().contains("/missing"));
}

#[test]
fn machine_status_distinguishes_path_aliases_from_virtual_objects() {
    let app = testdata_app();
    let before = status::status_json(&app, false).1;
    let before: serde_json::Value = serde_json::from_str(&before).unwrap();
    let canonical = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.ref_id.is_none())
        .cloned()
        .expect("canonical fixture item");
    {
        let mut catalog = app.catalog.write().unwrap();
        let mut path_alias = canonical.clone();
        path_alias.detail_id = catalog.next_detail;
        catalog.next_detail += 1;
        path_alias.object_id = "fixture-path-alias".into();
        path_alias.path = path_alias.path.with_file_name("fixture-hardlink-alias.mkv");
        catalog
            .by_detail
            .insert(path_alias.detail_id, path_alias.object_id.clone());
        catalog
            .items
            .insert(path_alias.object_id.clone(), path_alias.clone());

        let mut virtual_alias = path_alias;
        virtual_alias.object_id = "fixture-virtual-alias".into();
        virtual_alias.ref_id = Some("fixture-path-alias".into());
        catalog
            .items
            .insert(virtual_alias.object_id.clone(), virtual_alias);
    }
    let after = status::status_json(&app, false).1;
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    let before = &before["catalog"];
    let after = &after["catalog"];
    assert_eq!(after["physical_inodes"], before["physical_inodes"]);
    assert_eq!(
        after["path_aliases"].as_u64().unwrap(),
        before["path_aliases"].as_u64().unwrap() + 1
    );
    assert_eq!(
        after["media_records"].as_u64().unwrap(),
        before["media_records"].as_u64().unwrap() + 1
    );
    assert_eq!(
        after["item_objects"].as_u64().unwrap(),
        before["item_objects"].as_u64().unwrap() + 2
    );
    assert_eq!(after["container_objects"], before["container_objects"]);
    assert_eq!(
        after["total_objects"].as_u64().unwrap(),
        before["total_objects"].as_u64().unwrap() + 2
    );
}

#[test]
fn scan_workers_are_owned_report_progress_and_join_on_shutdown() {
    let test_tree = TestTree::new("scan-shutdown");
    let tmp = test_tree.path();
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    rusty_dlna_scan::write_fake_mkv(&media.join("movie.mkv"), 64);
    let app = Arc::new(App::from_config(
        Config {
            friendly_name: "scan-shutdown".into(),
            media_dir: vec![media.display().to_string()],
            cache_dir: Some(tmp.join("cache").display().to_string()),
            rescan_secs: 60,
            ..Config::default()
        },
        18200,
        11900,
        tmp,
    ));
    app.runtime_metrics
        .set_http_listener(ComponentState::Running);
    app.runtime_metrics.set_ssdp(ComponentState::Running);
    spawn_library_watch(app.clone()).unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, body) = status::status_json(&app, true);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        if value["scanner"]["worker_state"] == "running"
            && value["scanner"]["last_success_unix"].is_u64()
        {
            assert_eq!(value["status"], "healthy", "{body}");
            assert_eq!(value["listener"]["http"], "running");
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "workers did not become ready: {body}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    app.runtime_metrics
        .set_http_listener(ComponentState::Failed);
    let (failed_status, body) = status::status_json(&app, true);
    assert_eq!(failed_status, 503, "{body}");
    assert!(body.contains("HTTP accept loop is not running"), "{body}");
    let started = Instant::now();
    stop_library_watch(&app);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(app.scan_control.cancellation.is_cancelled());
    assert!(app.scan_control.threads.lock().unwrap().is_empty());
    let (seen, current) = app.scan_cfg.progress.as_ref().unwrap().snapshot();
    assert!(seen >= 1);
    assert!(current.is_some());
}

#[test]
fn startup_retries_a_failed_first_publication_without_events_or_periodic_rescan() {
    let test_tree = TestTree::new("startup-publication-retry");
    let tmp = test_tree.path();
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    rusty_dlna_scan::write_fake_mkv(&media.join("movie.mkv"), 64);
    let app = Arc::new(App::from_config(
        Config {
            friendly_name: "startup-publication-retry".into(),
            media_dir: vec![media.display().to_string()],
            cache_dir: Some(tmp.join("cache").display().to_string()),
            db_dir: Some(tmp.join("database").display().to_string()),
            rescan_secs: 0,
            ..Config::default()
        },
        18200,
        11900,
        tmp,
    ));
    let db_path = app.scan_cfg.db_path.as_ref().unwrap().clone();
    lifecycle::fail_prepared_publications_for_test(&db_path, 1);
    spawn_library_watch(Arc::clone(&app)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_retry = false;
    loop {
        let phase = app
            .scan_control
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            .clone();
        if phase == "initializing-retry" {
            saw_retry = true;
            assert!(
                app.scan_control.gate.try_lock().is_err(),
                "startup must retain the scan gate across retry backoff"
            );
        }
        if phase == "watching" && !read_recover(&app.catalog).items.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "startup retry did not converge: {phase}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        saw_retry,
        "the injected first publication failure was not observed"
    );
    assert!(
        LibraryDb::open(&db_path).unwrap().detail_count().unwrap() > 0,
        "the retry must publish without a later filesystem event"
    );
    assert_eq!(app.cfg.rescan_secs, 0);
    stop_library_watch(&app);
}

#[test]
fn startup_retry_backoff_is_woken_by_cancellation_before_watch_setup() {
    let test_tree = TestTree::new("startup-retry-cancel");
    let tmp = test_tree.path();
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    rusty_dlna_scan::write_fake_mkv(&media.join("movie.mkv"), 64);
    let app = Arc::new(App::from_config(
        Config {
            friendly_name: "startup-retry-cancel".into(),
            media_dir: vec![media.display().to_string()],
            cache_dir: Some(tmp.join("cache").display().to_string()),
            db_dir: Some(tmp.join("database").display().to_string()),
            rescan_secs: 0,
            ..Config::default()
        },
        18200,
        11900,
        tmp,
    ));
    let db_path = app.scan_cfg.db_path.as_ref().unwrap().clone();
    lifecycle::fail_prepared_publications_for_test(&db_path, usize::MAX);
    spawn_library_watch(Arc::clone(&app)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let phase = app
            .scan_control
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            .clone();
        if phase == "initializing-retry" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "startup did not enter retry backoff"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let started = Instant::now();
    stop_library_watch(&app);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "cancellation must wake startup backoff promptly: {:?}",
        started.elapsed()
    );
    assert_eq!(app.scan_telemetry.watch_count.load(Ordering::Acquire), 0);
    assert!(app.scan_control.threads.lock().unwrap().is_empty());
    lifecycle::clear_prepared_publication_failures_for_test(&db_path);
}

#[test]
fn scan_worker_join_never_exceeds_the_supplied_shutdown_deadline() {
    let test_tree = TestTree::new("scan-join-deadline");
    let app = App::from_config(
        Config {
            friendly_name: "scan-join-deadline".into(),
            cache_dir: Some(test_tree.path().join("cache").display().to_string()),
            rescan_secs: 0,
            ..Config::default()
        },
        18200,
        11900,
        test_tree.path(),
    );
    app.scan_control.threads.lock().unwrap().push(ScanWorker {
        role: "deliberately_blocked_test_worker",
        handle: std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
        }),
    });
    let started = Instant::now();
    stop_library_watch_until(&app, started + Duration::from_millis(40));
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(app.scan_control.threads.lock().unwrap().is_empty());
}

#[test]
fn search_samsung_v_finds_fixture() {
    let app = testdata_app();
    let tv = "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0";
    let (st, xml) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>V</ContainerID><SearchCriteria>dc:title contains "Fixture"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        tv,
    );
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("Fixture Movie"),
        "Search ContainerID=V must find Fixture Movie: {xml}"
    );
    assert!(
        xml.contains("/MediaItems/"),
        "Search ContainerID=V must list a media URL: {xml}"
    );
    let (st_img, img) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>I</ContainerID><SearchCriteria>dc:title contains "Fixture"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        tv,
    );
    assert_eq!(st_img, 200, "{img}");
    assert!(
        !img.contains("Fixture Movie"),
        "Search ContainerID=I must not return the video title: {img}"
    );
}

#[test]
fn search_xbox_exists_false_skips_refid_aliases() {
    let app = testdata_app();
    let xbox = "Xbox/360";
    let (st, xml) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>0</ContainerID><SearchCriteria>upnp:class derivedfrom "object.item.videoItem" and @refID exists false</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        xbox,
    );
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("Fixture Movie"),
        "Xbox exists false must still find the original: {xml}"
    );
    assert!(
        !xml.contains("refID="),
        "Xbox exists false must omit alias rows: {xml}"
    );
}

#[test]
fn search_or_matches_either_class() {
    let app = testdata_app();
    let (st, xml) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>0</ContainerID><SearchCriteria>(upnp:class derivedfrom "object.item.audioItem") or (upnp:class derivedfrom "object.item.videoItem")</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        "FormatMapTest/1.0",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("Fixture Movie"), "or must hit video: {xml}");
    assert!(xml.contains("Fixture Track"), "or must hit audio: {xml}");
}

#[test]
fn malformed_soap_arguments_and_search_criteria_return_standard_faults() {
    let app = testdata_app();

    for criteria in [
        r#"unknown:field contains "Fixture""#,
        r#"dc:title approximately "Fixture""#,
        r#"(dc:title contains "Fixture""#,
    ] {
        let (status, xml) = soap_action(
                &app,
                "Search",
                &format!(
                    "<ContainerID>0</ContainerID><SearchCriteria>{criteria}</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"
                ),
                "SearchFaultTest/1.0",
            );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>708</errorCode>"), "{xml}");
    }

    let (status, invalid_integer) = soap_action(
        &app,
        "Browse",
        r#"<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>not-a-number</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        "SoapFaultTest/1.0",
    );
    assert_eq!(status, 500, "{invalid_integer}");
    assert!(
        invalid_integer.contains("<errorCode>402</errorCode>"),
        "{invalid_integer}"
    );

    let body = br#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>0</ObjectID></s:Body></s:Envelope>"#;
    let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
    let mut request = HttpRequest::parse_headers(&raw).unwrap();
    request.body = body.to_vec();
    let response = app.handle(&request);
    let xml = String::from_utf8_lossy(&response.body);
    assert_eq!(response.status, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
}

#[test]
fn oversized_sort_criteria_faults_before_catalog_query() {
    let app = testdata_app();
    let db_path = app.scan_cfg.db_path.as_deref().expect("fixture DB path");
    let query_count = Arc::new(AtomicUsize::new(0));
    count_catalog_queries_for_test(db_path, Arc::clone(&query_count));
    let sort = std::iter::repeat_n("+dc:title", rusty_dlna_soap::MAX_SORT_KEYS + 1)
        .collect::<Vec<_>>()
        .join(",");

    for (action, prefix) in [
        (
            "Browse",
            "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>",
        ),
        (
            "Search",
            "<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria>",
        ),
    ] {
        let (status, xml) = soap_action(
            &app,
            action,
            &format!(
                "{prefix}<Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>1</RequestedCount><SortCriteria>{sort}</SortCriteria>"
            ),
            "DLNADOC/1.50",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>709</errorCode>"), "{xml}");
    }
    stop_counting_catalog_queries_for_test(db_path);
    assert_eq!(
        query_count.load(Ordering::Relaxed),
        0,
        "invalid sort criteria reached SQLite"
    );
}

#[test]
fn handler_rejects_manually_constructed_duplicate_soap_action() {
    let app = testdata_app();
    let request = HttpRequest {
        method: "POST".into(),
        target: "/ctl/ContentDir".into(),
        path: "/ctl/ContentDir".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: vec![
            ("Host".into(), "127.0.0.1".into()),
            ("SOAPAction".into(), "\"urn:x#Browse\"".into()),
            ("soapaction".into(), "\"urn:x#Search\"".into()),
        ],
        body: Vec::new(),
    };
    let response = app.handle(&request);
    assert_eq!(response.status, 400);
}

#[test]
fn browse_listed_filter_omits_res_size() {
    let app = testdata_app();
    let (st, xml) = soap_action(
        &app,
        "Browse",
        r#"<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>dc:title,upnp:class</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        "Kodi/21.0",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("Fixture Movie"), "{xml}");
    assert!(
        !xml.contains(" size=&quot;") && !xml.contains(" size=\""),
        "Filter without res@size: {xml}"
    );
    assert!(
        !xml.contains("&lt;res ") && !xml.contains("<res "),
        "Filter without res: {xml}"
    );
    let (st, star) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200);
    assert!(
        star.contains("size=&quot;") || star.contains("size=\""),
        "{star}"
    );
}

#[test]
fn browse_recent_keeps_mtime_order() {
    let app = testdata_app();
    {
        let mut cat = write_recover(&app.catalog);
        let mut videos: Vec<String> = cat
            .items
            .values()
            .filter(|i| {
                i.class.contains("video")
                    && i.ref_id.is_none()
                    && i.object_id
                        .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
            })
            .map(|i| i.object_id.clone())
            .collect();
        videos.sort();
        assert!(
            videos.len() >= 2,
            "need two browse-folder videos, got {videos:?}"
        );
        let older = videos[0].clone();
        let newer = videos[1].clone();
        if let Some(it) = cat.items.get_mut(&older) {
            it.title = "Aaa Early".into();
            it.mtime = 1;
        }
        if let Some(it) = cat.items.get_mut(&newer) {
            it.title = "Zzz Fresh".into();
            it.mtime = 9_999_999;
        }
        cat.recent_ids = vec![newer, older];
        cat.recent_count = 2;
    }
    let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    let fresh = xml.find("Zzz Fresh").expect("fresh title in recent DIDL");
    let early = xml.find("Aaa Early").expect("old title in recent DIDL");
    assert!(
        fresh < early,
        "Recently Added must stay mtime-desc, not title: {xml}"
    );

    let (st, titled) = soap_action(
            &app,
            "Browse",
            "<ObjectID>2$FF0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>",
            "Kodi/21.0",
        );
    assert_eq!(st, 200, "{titled}");
    let early = titled.find("Aaa Early").expect("old title after +dc:title");
    let fresh = titled
        .find("Zzz Fresh")
        .expect("fresh title after +dc:title");
    assert!(
        early < fresh,
        "explicit +dc:title must still re-sort Recent: {titled}"
    );
}

#[test]
fn browse_force_sort_track_order() {
    let mut app = testdata_app();
    app.scan_cfg.db_path = None;
    {
        let mut cat = write_recover(&app.catalog);
        let mut videos: Vec<String> = cat
            .items
            .values()
            .filter(|i| {
                i.class.contains("video")
                    && i.ref_id.is_none()
                    && i.object_id
                        .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
            })
            .map(|i| i.object_id.clone())
            .collect();
        videos.sort();
        assert!(
            videos.len() >= 2,
            "need two browse-folder videos, got {videos:?}"
        );
        let high_oid = videos[0].clone();
        let low_oid = videos[1].clone();
        let high_detail = cat.items.get(&high_oid).unwrap().detail_id;
        let low_detail = cat.items.get(&low_oid).unwrap().detail_id;
        for it in cat.items.values_mut() {
            if it.detail_id == high_detail {
                it.title = "Aaa HighTrack".into();
                it.disc = Some(2);
                it.track = Some(5);
            } else if it.detail_id == low_detail {
                it.title = "Zzz LowTrack".into();
                it.disc = Some(1);
                it.track = Some(1);
            }
        }
    }
    let (st, xml) = soap_browse(
        &app,
        "2$8",
        "BrowseDirectChildren",
        "Panasonic DLNADOC/1.50",
    );
    assert_eq!(st, 200, "{xml}");
    let low = xml.find("Zzz LowTrack").expect("Zzz LowTrack in DIDL");
    let high = xml.find("Aaa HighTrack").expect("Aaa HighTrack in DIDL");
    assert!(
        low < high,
        "FORCE_SORT disc/track must put Zzz LowTrack before Aaa HighTrack: {xml}"
    );
    let (st709, body709) = soap_action(
            &app,
            "Browse",
            "<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+notAField</SortCriteria>",
            "DLNADOC/1.50",
        );
    assert_eq!(st709, 500, "{body709}");
    assert!(body709.contains("709"), "{body709}");
}

#[test]
fn cache_keeps_kodi_when_generic_ua_follows() {
    let app = testdata_app();
    let peer: SocketAddr = "192.0.2.40:1234".parse().unwrap();
    let (st, kodi) = {
        let body = r#"<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#;
        let envelope = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">{body}</u:Browse></s:Body></s:Envelope>"#
        );
        let raw = format!(
                "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                envelope.len()
            );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = envelope.into_bytes();
        let r = app.handle_from(&req, peer);
        (r.status, String::from_utf8_lossy(&r.body).into_owned())
    };
    assert_eq!(st, 200);
    assert!(kodi.contains("/Captions/"), "{kodi}");
    assert!(!kodi.contains("/Transcode/"));

    let movie = movie_fixture(&app);
    let gen = app.handle_from(
        &req(&get(
            &format!("/MediaItems/{}.mkv", movie.detail_id),
            "DLNADOC/1.50",
        )),
        peer,
    );
    assert_eq!(gen.status, 200);

    let cr_peer: SocketAddr = "192.0.2.41:1234".parse().unwrap();
    let envelope = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
    let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54.384650 DLNADOC/1.50\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            envelope.len()
        );
    let mut creq = HttpRequest::parse_headers(&raw).unwrap();
    creq.body = envelope.into_bytes();
    let cr = app.handle_from(&creq, cr_peer);
    let cr_xml = String::from_utf8_lossy(&cr.body);
    assert!(cr_xml.contains("/Transcode/"), "{cr_xml}");
    let tid = cr_xml
        .split("/Transcode/")
        .nth(1)
        .and_then(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<i64>()
                .ok()
        })
        .expect("transcode id");
    let tget = app.handle_from(
        &req(&get(&format!("/Transcode/{tid}.mp4"), "DLNADOC/1.50")),
        cr_peer,
    );
    assert_ne!(
        tget.status, 404,
        "cached CrKey must still remap Transcode GET"
    );
}

#[test]
fn cache_expires_after_one_hour() {
    let app = testdata_app();
    let peer: SocketAddr = "192.0.2.42:9".parse().unwrap();
    let ip: Ipv4Addr = "192.0.2.42".parse().unwrap();
    let _ = app.handle_from(&req(&get("/rootDesc.xml", "Kodi/21.0")), peer);
    {
        let mut cache = app.client_cache.lock().unwrap();
        cache.set_age(ip, 0);
    }
    assert!(
        app.client_cache
            .lock()
            .unwrap()
            .search(ip, 3601, None)
            .is_none(),
        "expired without MAC"
    );
    let _ = app.handle_from(&req(&get("/rootDesc.xml", "Kodi/21.0")), peer);
    {
        let mut cache = app.client_cache.lock().unwrap();
        cache.set_age(ip, 0);
    }
    assert!(
        app.client_cache
            .lock()
            .unwrap()
            .search(ip, 3601, Some([1, 2, 3, 4, 5, 6]))
            .is_none(),
        "HTTP stores no MAC"
    );
    {
        let kodi = identify_user_agent("Kodi/21.0").expect("kodi");
        let mut cache = app.client_cache.lock().unwrap();
        cache.remember(ip, kodi, Some([1, 2, 3, 4, 5, 6]), 0);
        assert_eq!(
            cache
                .search(ip, 3601, Some([1, 2, 3, 4, 5, 6]))
                .map(|p| p.kind),
            Some(ClientKind::Kodi)
        );
    }
}

#[test]
fn pfs_xbox_browse_eight_is_video_all() {
    let app = testdata_app();
    let (st8, eight) = soap_browse(&app, "8", "BrowseDirectChildren", "Xbox/360");
    let (st, all) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Xbox/360");
    assert_eq!(st8, 200, "{eight}");
    assert_eq!(st, 200, "{all}");
    assert!(
        eight.contains("/MediaItems/"),
        "Xbox Browse 8 must remap to Video All: {eight}"
    );
    assert!(
        eight.contains("NumberReturned") && all.contains("NumberReturned"),
        "both pages return items"
    );
    let n8 = xml_tag_text(&eight, "NumberReturned").unwrap_or_default();
    let nall = xml_tag_text(&all, "NumberReturned").unwrap_or_default();
    assert_eq!(n8, nall, "8 and 2$8 must return the same page");
}

#[test]
fn feature_list_dcm10_and_root_container_collapse() {
    let mut app = testdata_app();
    let tv = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
    assert!(tv.contains("id=&quot;A&quot;"), "{tv}");
    assert!(tv.contains("id=&quot;V&quot;"), "{tv}");
    assert!(tv.contains("id=&quot;I&quot;"), "{tv}");
    app.cfg.root_container = Some("V".into());
    let collapsed = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
    let video_hits = collapsed.matches("id=&quot;2&quot;").count()
        + collapsed.matches("id=&quot;V&quot;").count();
    assert!(
        video_hits >= 3,
        "non-64 root_container collapses FeatureList: {collapsed}"
    );
    app.cfg.root_container = Some("64".into());
    let folders = feature_list(&app, "Kodi/21.0");
    assert!(folders.contains("id=&quot;1$14&quot;"), "{folders}");
    assert!(folders.contains("id=&quot;2$15&quot;"), "{folders}");
    assert!(folders.contains("id=&quot;3$16&quot;"), "{folders}");
}

#[test]
fn setbookmark_missing_object_is_701() {
    let app = testdata_app();
    let (st, xml) = soap_action(
        &app,
        "X_SetBookmark",
        "<ObjectID>no-such-object</ObjectID><PosSecond>90</PosSecond>",
        "Kodi/21.0",
    );
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
}

#[test]
fn search_negative_starting_index_is_402() {
    let app = testdata_app();
    let (st, xml) = soap_action(
        &app,
        "Search",
        r#"<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>-1</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        "Kodi/21.0",
    );
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
}

#[test]
fn search_missing_container_id_is_402() {
    let app = testdata_app();
    let (status, xml) = soap_action(
        &app,
        "Search",
        r#"<SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
        "Kodi/21.0",
    );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
}

#[test]
fn browse_and_search_require_explicit_paging_arguments() {
    let app = testdata_app();
    for (action, body) in [
        (
            "Browse",
            "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>",
        ),
        (
            "Browse",
            "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><SortCriteria></SortCriteria>",
        ),
        (
            "Search",
            "<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>",
        ),
        (
            "Search",
            "<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><SortCriteria></SortCriteria>",
        ),
    ] {
        let (status, xml) = soap_action(&app, action, body, "PagingArgumentTest/1.0");
        assert_eq!(status, 500, "{action}: {xml}");
        assert!(
            xml.contains("<errorCode>402</errorCode>"),
            "{action}: {xml}"
        );
    }
}

#[test]
fn connection_and_registrar_required_arguments_reach_http_faults() {
    let app = testdata_app();
    for body in ["", "<ConnectionID>invalid</ConnectionID>"] {
        let (status, xml) = soap_action(&app, "GetCurrentConnectionInfo", body, "Kodi/21.0");
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    }
    let (status, xml) = soap_action(
        &app,
        "GetCurrentConnectionInfo",
        "<ConnectionID>1</ConnectionID>",
        "Kodi/21.0",
    );
    assert_eq!(status, 500, "{xml}");
    assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
    let (status, xml) = soap_action(
        &app,
        "GetCurrentConnectionInfo",
        "<ConnectionID>0</ConnectionID>",
        "Kodi/21.0",
    );
    assert_eq!(status, 200, "{xml}");

    for method in ["IsAuthorized", "IsValidated"] {
        let (status, xml) = soap_action(&app, method, "", "Kodi/21.0");
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
        let (status, xml) = soap_action(
            &app,
            method,
            "<DeviceID>uuid:client</DeviceID>",
            "Kodi/21.0",
        );
        assert_eq!(status, 200, "{xml}");
    }
}

#[test]
fn query_state_variable_connection_status_missing_unknown() {
    let app = testdata_app();
    let (st, xml) = soap_action(
        &app,
        "QueryStateVariable",
        "<varName>ConnectionStatus</varName>",
        "Kodi/21.0",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("<return>Connected</return>"), "{xml}");
    let (st, xml) = soap_action(&app, "QueryStateVariable", "", "Kodi/21.0");
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    let (st, xml) = soap_action(
        &app,
        "QueryStateVariable",
        "<varName>NotAVariable</varName>",
        "Kodi/21.0",
    );
    assert_eq!(st, 500, "{xml}");
    assert!(xml.contains("<errorCode>404</errorCode>"), "{xml}");
}

#[test]
fn browse_metadata_root_includes_search_class() {
    let app = testdata_app();
    let (st, meta) = soap_browse(&app, "0", "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{meta}");
    assert!(
        meta.contains("searchClass") && meta.contains("includeDerived"),
        "{meta}"
    );
    assert!(meta.contains("object.item.audioItem"), "{meta}");
    assert!(meta.contains("object.item.imageItem"), "{meta}");
    assert!(meta.contains("object.item.videoItem"), "{meta}");
}

#[test]
fn didl_alias_items_have_refid() {
    let app = testdata_app();
    let alias = {
        let cat = app.catalog.read().unwrap();
        cat.items
            .values()
            .find(|i| i.ref_id.as_ref().is_some_and(|r| !r.is_empty()))
            .cloned()
            .expect("virtual alias with REF_ID")
    };
    let (st, xml) = soap_browse(&app, &alias.object_id, "BrowseMetadata", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    let rid = alias.ref_id.as_deref().unwrap();
    assert!(
        xml.contains(&format!("refID=\"{rid}\""))
            || xml.contains(&format!("refID=&quot;{rid}&quot;")),
        "alias {} missing refID={rid}: {xml}",
        alias.object_id
    );
}

#[test]
fn music_and_image_virtuals_browse_ok() {
    let app = testdata_app();
    for oid in ["1", "3", "1$4", "3$B", "1$FF0", "3$FF0"] {
        let (st, xml) = soap_browse(&app, oid, "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{oid} {xml}");
        assert!(xml.contains("BrowseResponse"), "{oid} {xml}");
        assert!(
            !xml.contains("<errorCode>"),
            "virtual {oid} must not fault: {xml}"
        );
    }
    let (st, music) = soap_browse(&app, "1", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{music}");
    assert!(
        music.contains("id=\"1$4\"")
            || music.contains("All Music")
            || music.contains("Recently Added"),
        "{music}"
    );
    let (st, pics) = soap_browse(&app, "3", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{pics}");
    assert!(
        pics.contains("id=\"3$B\"")
            || pics.contains("All Pictures")
            || pics.contains("Recently Added"),
        "{pics}"
    );
    let (st, tracks) = soap_browse(&app, "1$4", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{tracks}");
    assert!(
        tracks.contains("Fixture Track"),
        "All Music must list the FLAC fixture: {tracks}"
    );
}

#[test]
fn checked_fixtures_match_scan_didl_and_get_contract() {
    let app = testdata_app();
    let expected = [
        (
            "music/song.flac",
            "audio/x-flac",
            "item.audioItem.musicTrack",
            "Fixture Track",
        ),
        (
            "music/song.mp3",
            "audio/mpeg",
            "item.audioItem.musicTrack",
            "Fixture Track",
        ),
        ("video/tagged.mp4", "video/mp4", "item.videoItem", "tagged"),
        (
            "pictures/shot.jpg",
            "image/jpeg",
            "item.imageItem.photo",
            "Fixture Photo",
        ),
    ];

    for (relative, mime, class, title) in expected {
        let item = {
            let catalog = app.catalog.read().unwrap();
            catalog
                .items
                .values()
                .find(|item| item.ref_id.is_none() && item.path.ends_with(relative))
                .cloned()
                .unwrap_or_else(|| panic!("checked fixture was not indexed: {relative}"))
        };
        assert_eq!(item.mime, mime, "{relative}");
        assert_eq!(item.class, class, "{relative}");
        assert_eq!(item.title, title, "{relative}");

        let (status, didl) = soap_browse(&app, &item.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(status, 200, "{relative}: {didl}");
        assert!(didl.contains(class), "{relative}: {didl}");
        assert!(
            didl.contains(&format!("http-get:*:{mime}:")),
            "{relative}: {didl}"
        );

        let response = app.handle(&req(&get(
            &format!("/MediaItems/{}.{}", item.detail_id, item.ext),
            "FixtureParity/1.0",
        )));
        assert_eq!(response.status, 200, "GET {relative}");
        assert_eq!(resp_header(&response, "Content-Type"), Some(mime));
        let expected_length = item.size.to_string();
        assert_eq!(
            resp_header(&response, "Content-Length"),
            Some(expected_length.as_str())
        );
        if response.file_range.is_none() {
            assert_eq!(response.body, std::fs::read(&item.path).unwrap());
        } else {
            assert_eq!(
                response
                    .file_range
                    .as_ref()
                    .map(|range| range.path.as_path()),
                Some(item.path.as_path())
            );
        }
    }

    let song = app
        .catalog
        .read()
        .unwrap()
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path.ends_with("music/song.flac"))
        .cloned()
        .unwrap();
    // Sidecar `studio` intentionally overrides embedded `artist`; the
    // remaining embedded fields survive the NFO merge.
    assert_eq!(song.artist.as_deref(), Some("Fixture Band"));
    assert_eq!(song.album_artist.as_deref(), Some("Fixture Album Artist"));
    assert_eq!(song.album.as_deref(), Some("Fixture Album"));
    assert_eq!(song.composer.as_deref(), Some("Fixture Composer"));
    assert_eq!(song.track, Some(2));
    assert_eq!(song.disc, Some(1));
}

#[test]
fn recent_keeps_old_mtime_and_caps_at_200() {
    let app = testdata_app();
    let movie = {
        let cat = app.catalog.read().unwrap();
        cat.items
            .values()
            .find(|i| {
                i.path.ends_with("movie.mkv")
                    && i.ref_id.is_none()
                    && i.object_id
                        .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
            })
            .cloned()
            .expect("browse-folder movie.mkv")
    };
    {
        let mut cat = write_recover(&app.catalog);
        if let Some(it) = cat.items.get_mut(&movie.object_id) {
            it.mtime = 1;
            it.ref_id = None;
        }
        cat.recent_ids.clear();
        cat.recent_count = 0;
    }
    let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("Fixture Movie") || xml.contains(&movie.title),
        "old-mtime files stay in Recent (no 90-day window): {xml}"
    );
    {
        let mut cat = write_recover(&app.catalog);
        for i in 0..210i64 {
            let oid = format!("64$RECENTTEST${i:X}");
            let mut clone = movie.clone();
            clone.object_id = oid.clone();
            clone.parent_id = "64".into();
            clone.ref_id = None;
            clone.detail_id = 50_000 + i;
            clone.title = format!("cap{i:03}");
            clone.mtime = 2_000_000_000 + i;
            clone.inode = 80_000 + i as u64;
            clone.device = 9;
            cat.items.insert(oid, clone);
        }
        cat.recent_ids.clear();
        cat.recent_count = 0;
    }
    let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
    assert_eq!(st, 200, "{xml}");
    let returned: u32 = xml_tag_text(&xml, "NumberReturned")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let total: u32 = xml_tag_text(&xml, "TotalMatches")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        returned <= 200 && total <= 200,
        "RECENT_MAX is 200 unique items: returned={returned} total={total}"
    );
    assert_eq!(total, 200, "cap is exactly 200 with 210+ videos: {xml}");
}

#[test]
fn getcontentfeatures_not_one_is_400() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let bad = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ngetcontentFeatures.dlna.org: 0\r\n\r\n",
            movie.detail_id
        );
    assert_eq!(app.handle(&req(&bad)).status, 400);
    let ok = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n",
            movie.detail_id
        );
    assert_eq!(app.handle(&req(&ok)).status, 200);
}

#[test]
fn interactive_on_non_image_is_406_except_samsung() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let kodi = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ntransferMode.dlna.org: Interactive\r\n\r\n",
            movie.detail_id
        );
    assert_eq!(app.handle(&req(&kodi)).status, 406);
    let samsung = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\ntransferMode.dlna.org: Interactive\r\n\r\n",
            movie.detail_id
        );
    assert_eq!(app.handle(&req(&samsung)).status, 200);
    let strict = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\ntransferMode.dlna.org: Interactive\r\nuctt.upnp.org: 1\r\n\r\n",
            movie.detail_id
        );
    assert_eq!(app.handle(&req(&strict)).status, 406);
}

#[test]
fn skip_dlna_pn_omits_pn_on_http_content_features() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    set_detail_dlna_pn(&app, movie.detail_id, "AVC_MP4_MP_SD_AC3");
    let kodi = app.handle(&req(&format!(
        "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n",
        movie.detail_id
    )));
    assert_eq!(kodi.status, 200);
    let kfeats = resp_header(&kodi, "contentFeatures.dlna.org").unwrap_or("");
    assert!(
        kfeats.contains("DLNA.ORG_PN=AVC_MP4_MP_SD_AC3"),
        "Kodi keeps PN on the by_detail item: {kfeats}"
    );
    let j5500 = app.handle(&req(&format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 [BD]J5500\r\n\r\n",
            movie.detail_id
        )));
    assert_eq!(j5500.status, 200);
    let feats = resp_header(&j5500, "contentFeatures.dlna.org").unwrap_or("");
    assert!(
        !feats.contains("DLNA.ORG_PN="),
        "J5500 SKIP_DLNA_PN must omit PN on contentFeatures: {feats}"
    );
    assert!(feats.contains("DLNA.ORG_OP=01"), "{feats}");
}

#[test]
fn lg_browse_captioned_title_ends_with_dot() {
    let app = testdata_app();
    let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "LGE_DLNA_SDK/1.6.0");
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("Fixture Movie."),
        "LG caption hack appends '.': {xml}"
    );
}

#[test]
fn toshiba_browse_extra_ci1_res() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    set_detail_dlna_pn(&app, movie.detail_id, "MPEG_TS_HD_NA");
    let oid = {
        let cat = app.catalog.read().unwrap();
        cat.by_detail
            .get(&movie.detail_id)
            .cloned()
            .expect("by_detail oid")
    };
    let (st, xml) = soap_browse(
        &app,
        &oid,
        "BrowseMetadata",
        "UPnP/1.0 DLNADOC/1.50 Intel_SDK_for_UPnP_devices/1.2",
    );
    assert_eq!(st, 200, "{xml}");
    assert!(
        xml.contains("DLNA.ORG_PN=MPEG_PS_NTSC") && xml.contains("DLNA.ORG_CI=1"),
        "Toshiba extra CI=1 res: {xml}"
    );
}

#[test]
fn sony_bdp_get_remaps_mkv_to_divx() {
    let app = testdata_app();
    let movie = movie_fixture(&app);
    let raw = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: UPnP/1.0 DLNADOC/1.50\r\nX-AV-Client-Info: av=\"5.0\"; cn=\"Sony Corporation\"; mv=\"2.0\"\r\n\r\n",
            movie.detail_id
        );
    let r = app.handle(&req(&raw));
    assert_eq!(r.status, 200);
    assert_eq!(resp_header(&r, "Content-Type"), Some("video/divx"));

    let (st, xml) = {
        let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
        let hdr = format!(
                "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nX-AV-Client-Info: av=\"5.0\"; mv=\"2.0\"\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                body.len()
            );
        let mut req = HttpRequest::parse_headers(&hdr).unwrap();
        req.body = body.into_bytes();
        let r = app.handle(&req);
        (r.status, String::from_utf8_lossy(&r.body).into_owned())
    };
    assert_eq!(st, 200, "{xml}");
    assert!(xml.contains("video/divx"), "primary remapped mime: {xml}");
    assert!(
        xml.contains("MPEG_PS_NTSC") && xml.contains("DLNA.ORG_CI=1"),
        "extra CI=1 still uses original mkv mime: {xml}"
    );
}
