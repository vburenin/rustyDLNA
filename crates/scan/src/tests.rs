use super::*;

pub(super) struct TempPath(PathBuf);

impl TempPath {
    pub(super) fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "rusty-dlna-{label}-{}-{sequence}",
            std::process::id()
        )))
    }
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[test]
fn seeded_virtual_views_match_rustydlna_contract() {
    let catalog = Catalog::new();

    for (id, title) in [
        (MUSIC_RECENT_ID, "Recently Added"),
        (VIDEO_RECENT_ID, "Recently Added"),
        (IMAGE_RECENT_ID, "Recently Added"),
        (MUSIC_ALL_ID, "All Music"),
        (VIDEO_ALL_ID, "All Video"),
        (IMAGE_ALL_ID, "All Pictures"),
        (MUSIC_GENRE_ID, "Genre"),
        (MUSIC_ARTIST_ID, "Artist"),
        (MUSIC_ALBUM_ID, "Album"),
        (IMAGE_DATE_ID, "Date Taken"),
        (IMAGE_CAMERA_ID, "Camera"),
    ] {
        let container = catalog
            .containers
            .get(id)
            .unwrap_or_else(|| panic!("catalog missing virtual view {id}"));
        assert_eq!(container.title, title, "title for virtual view {id}");
    }

    for (alias, target) in [
        ("4", MUSIC_ALL_ID),
        ("5", MUSIC_GENRE_ID),
        ("6", MUSIC_ARTIST_ID),
        ("7", MUSIC_ALBUM_ID),
        ("8", VIDEO_ALL_ID),
        ("B", IMAGE_ALL_ID),
        ("C", IMAGE_DATE_ID),
        ("14", MUSIC_DIR_ID),
        ("15", VIDEO_DIR_ID),
        ("16", IMAGE_DIR_ID),
        ("D2", IMAGE_CAMERA_ID),
    ] {
        assert!(!alias.is_empty());
        assert!(catalog.containers.contains_key(target));
    }
}

#[test]
fn text_named_mkv_is_not_viable() {
    let p = TempPath::new("not-video.mkv");
    std::fs::write(&p, b"this is a readme pretending to be a movie\n").unwrap();
    assert!(!looks_like_av_container(&p));
    assert!(!file_is_viable(&p));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn targeted_ambiguous_format_probe_obeys_the_global_helper_gate() {
    let temp = TempPath::new("ambiguous-helper-gate");
    std::fs::create_dir_all(&temp).unwrap();
    let media = temp.join("ambiguous.mp4");
    write_fake_mkv(&media, 64);
    let db = LibraryDb::open(&temp.join("files.db")).unwrap();
    let gate = std::sync::Arc::new(HelperGate::new(1, 1));
    let cfg = ScanConfig {
        media_dirs: vec![temp.clone()],
        types: MediaTypes::all(),
        helper_gate: Some(gate.clone()),
        helper_queue_timeout: std::time::Duration::from_millis(20),
        ..Default::default()
    };

    let held = gate.try_acquire().unwrap();
    let error = index_one_file(&db, &cfg, &media, BROWSEDIR_ID).unwrap_err();
    assert!(matches!(
        error,
        ScanError::HelperAdmission(HelperAdmissionError::TimedOut)
    ));
    assert!(db
        .find_detail_by_path(&path_to_db(&media))
        .unwrap()
        .is_none());

    drop(held);
    assert!(index_one_file(&db, &cfg, &media, BROWSEDIR_ID).unwrap());
    assert!(db
        .find_detail_by_path(&path_to_db(&media))
        .unwrap()
        .is_some());
}

#[test]
fn guess_hdr_from_name_disc_remux_vs_web() {
    assert_eq!(
        guess_hdr_from_name("01 - Despicable Me (2010) - 2160p UHD BluRay Remux"),
        None,
        "no DV token"
    );
    assert_eq!(
        guess_hdr_from_name("04 - Despicable Me 4 (2024) - 2160p UHD BDRemux HDR DV"),
        Some("dv-p7")
    );
    assert_eq!(
        guess_hdr_from_name("Movie.2024.2160p.UHD.BDRemux.HDR.DV.HEVC"),
        Some("dv-p7")
    );
    assert_eq!(
        guess_hdr_from_name("Show.S01E01.2160p.WEB-DL.DDP5.1.DV.H.265"),
        None,
        "WEB-DL DoVi is usually P8"
    );
    assert_eq!(
        guess_hdr_from_name("02 - Example (2019) - 2160p UHD BDRemux Hybrid DoVi"),
        None,
        "Hybrid DoVi remux is usually P8"
    );
    assert_eq!(guess_hdr_from_name("clip.dv-p7.mkv"), Some("dv-p7"));
    assert_eq!(guess_hdr_from_name("clip.dv-p8.mkv"), Some("dv-p8"));
}

#[test]
fn page_children_matches_children_of_slice() {
    let mut cat = Catalog::new();
    cat.add_container(
        "64$1",
        BROWSEDIR_ID,
        "video",
        "container.storageFolder",
        true,
    );
    cat.link_child(BROWSEDIR_ID, "64$1");
    for i in 0..20 {
        let oid = format!("64$1${i:X}");
        cat.items.insert(
            oid.clone(),
            MediaItem {
                object_id: oid.clone(),
                parent_id: "64$1".into(),
                detail_id: i + 1,
                title: format!("m{i:02}"),
                class: "item.videoItem".into(),
                date: "2024-01-01".into(),
                path: PathBuf::from(format!("/m/{i}.mkv")),
                collection_path: None,
                mime: "video/x-matroska".into(),
                ext: "mkv".into(),
                size: 1000,
                mtime: 1,
                captions: vec![],
                probe: SourceProbe::default(),
                dlna_pn: None,
                ref_id: None,
                device: 1,
                inode: i as u64 + 1,
                duration: None,
                bitrate: None,
                resolution: None,
                channels: None,
                samplerate: None,
                album_art: 0,
                creator: None,
                about: None,
                plot: None,
                artist: None,
                album_artist: None,
                composer: None,
                contributor: None,
                album: None,
                genre: None,
                disc: None,
                track: None,
                rating: None,
                rotation: None,
                bookmark_sec: 0,
                watch_count: 0,
            },
        );
        cat.link_child("64$1", &oid);
    }
    let all = cat.children_of("64$1").unwrap();
    let (page, total) = cat.page_children("64$1", 5, 7).unwrap();
    assert_eq!(total, all.len() as u32);
    assert_eq!(page.len(), 7);
    for (a, b) in page.iter().zip(all.iter().skip(5)) {
        match (a, b) {
            (CatalogChild::Item(x), CatalogChild::Item(y)) => {
                assert_eq!(x.object_id, y.object_id);
            }
            _ => panic!("expected items"),
        }
    }
}

#[test]
fn skip_rules() {
    assert!(is_junk_dir("@eaDir"));
    assert!(is_sample_or_trailer_dir("sample"));
    assert!(!is_sample_or_trailer_dir("Sample"));
    assert!(is_unfinished_name("movie.mkv.part"));
    assert!(looks_like_sample_file("Movie-sample.mkv"));
    assert!(is_disc_structure_dir("BDMV"));
    assert!(is_disc_structure_dir("bdmv"));
    assert!(is_disc_structure_dir("VIDEO_TS"));
    assert!(is_skipped_dir("CERTIFICATE"));
    assert!(is_skipped_dir(".rusty_previews"));
    assert!(is_skipped_dir("movie.rustydlna-previews"));
    assert!(!is_skipped_dir("Movies"));
}

#[test]
fn generated_trickplay_directories_are_never_catalogued() {
    let root = TempPath::new("trickplay-skip");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("movie.mkv");
    write_fake_mkv(&source, 64);
    let preview_dir = rusty_dlna_protocol::trickplay_directory_for_media(&source).unwrap();
    std::fs::create_dir_all(&preview_dir).unwrap();
    write_fake_mkv(&preview_dir.join("must-not-appear.mp4"), 64);

    let catalog = scan(&ScanConfig {
        media_dirs: vec![root.clone()],
        types: MediaTypes::all(),
        thumbnails: false,
        include_hidden: true,
        ..Default::default()
    })
    .unwrap();
    assert!(catalog.items.values().any(|item| item.path == source));
    assert!(catalog
        .items
        .values()
        .all(|item| !item.path.starts_with(&preview_dir)));
}

#[test]
fn caption_compatibility_wrappers_follow_the_protocol_table() {
    for format in rusty_dlna_protocol::CAPTION_FORMATS {
        let uppercase = format.extension.to_ascii_uppercase();
        let name = format!("movie.{uppercase}");
        assert!(is_caption_name(&name), "{} admission", format.extension);
        assert_eq!(caption_ext(&name), format.extension);
        assert_eq!(caption_http_mime(&uppercase), format.http_mime);
    }
    assert!(!is_caption_name("movie.txt"));
    assert_eq!(caption_ext("movie.txt"), "sub");
    assert_eq!(caption_http_mime("unknown"), "text/plain");
}

#[test]
fn every_protocol_caption_format_uses_the_same_association_grammar() {
    let media = Path::new("/media/movie.mkv");
    for format in rusty_dlna_protocol::CAPTION_FORMATS {
        let uppercase = format.extension.to_ascii_uppercase();
        let exact = PathBuf::from(format!("/media/movie.{uppercase}"));
        let language = PathBuf::from(format!("/media/movie.en.{uppercase}"));
        let unrelated = PathBuf::from(format!("/media/other.en.{uppercase}"));
        assert!(caption_path_matches_media(&exact, media), "{exact:?}");
        assert!(caption_path_matches_media(&language, media), "{language:?}");
        assert!(
            !caption_path_matches_media(&unrelated, media),
            "{unrelated:?}"
        );
        assert_eq!(caption_language_for_media(&exact, media), None);
        assert_eq!(
            caption_language_for_media(&language, media),
            Some("en".into())
        );
    }
}

#[cfg(unix)]
#[test]
fn caption_association_and_language_share_raw_stem_ownership() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let path = |stem_byte, suffix: &[u8]| {
        let mut name = b"movie-".to_vec();
        name.push(stem_byte);
        name.extend_from_slice(suffix);
        PathBuf::from(OsString::from_vec(name))
    };
    let media = path(0x80, b".mkv");
    let exact = path(0x80, b".SrT");
    let language = path(0x80, b".EN.forced.SsA");
    let language_region = path(0x80, b".en-US.srt");
    let language_role = path(0x80, b".eng_forced.ass");
    let hyphen = path(0x80, b"-en.srt");
    let underscore = path(0x80, b"_en.srt");
    let other_stem = path(0x81, b".en.srt");
    let unknown = path(0x80, b".en.txt");

    assert!(caption_path_matches_media(&exact, &media));
    assert!(caption_path_matches_media(&language, &media));
    assert_eq!(caption_language_for_media(&exact, &media), None);
    assert_eq!(
        caption_language_for_media(&language, &media),
        Some("en".into())
    );
    assert_eq!(
        caption_language_for_media(&language_region, &media),
        Some("en".into())
    );
    assert_eq!(
        caption_language_for_media(&language_role, &media),
        Some("eng".into())
    );
    for unowned in [&hyphen, &underscore, &other_stem, &unknown] {
        assert!(!caption_path_matches_media(unowned, &media), "{unowned:?}");
        assert_eq!(caption_language_for_media(unowned, &media), None);
    }

    let captions = captions_from_candidates(&media, &[language.clone(), exact.clone()]);
    assert_eq!(
        captions
            .iter()
            .map(|caption| caption.ext.as_str())
            .collect::<Vec<_>>(),
        ["ssa", "srt"]
    );
}

#[test]
fn fake_mkv_writer_keeps_the_ebml_fallback_when_ffmpeg_rejects_the_output() {
    let tmp = TempPath::new("fake-mkv-fallback");
    let path = tmp.join("fixture.rusty-dlna-unknown");
    write_fake_mkv(&path, 9);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 9);
    assert_eq!(&bytes[..4], &[0x1a, 0x45, 0xdf, 0xa3]);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn exclusion_hidden_and_sidecar_policy_options_are_effective() {
    assert!(basename_glob_matches("*-extra.?kv", "Movie-EXTRA.mkv"));
    assert!(basename_glob_matches("foo?.mkv", "FOO1.MKV"));
    assert!(!basename_glob_matches("foo?.mkv", "foo12.mkv"));

    let tmp = TempPath::new("scan-policy");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".hidden-dir")).unwrap();
    for path in [
        tmp.join("keep.mkv"),
        tmp.join("movie-extra.mkv"),
        tmp.join(".hidden.mkv"),
        tmp.join(".hidden-dir/inside.mkv"),
    ] {
        write_fake_mkv(&path, 64);
    }
    std::fs::write(tmp.join("keep.en.srt"), "caption").unwrap();
    std::fs::write(tmp.join("MyArt.jpg"), TINY_JPEG).unwrap();
    let base = ScanConfig {
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        exclude_files: vec!["*-extra.*".into()],
        album_art_names: vec!["MyArt.jpg".into()],
        subtitles: false,
        thumbnails: false,
        ..Default::default()
    };
    assert!(is_album_art_name_for_config("myart.JPG", &base));
    let catalog = scan(&base).unwrap();
    let originals: Vec<_> = catalog
        .items
        .values()
        .filter(|item| item.ref_id.is_none())
        .collect();
    assert_eq!(originals.len(), 1, "{originals:#?}");
    assert_eq!(originals[0].path, tmp.join("keep.mkv"));
    assert!(originals[0].captions.is_empty());
    assert!(
        originals[0].album_art > 0,
        "configured sidecar remains enabled"
    );

    let visible = ScanConfig {
        include_hidden: true,
        album_art_names: Vec::new(),
        ..base
    };
    let catalog = scan(&visible).unwrap();
    let paths: HashSet<_> = catalog
        .items
        .values()
        .filter(|item| item.ref_id.is_none())
        .map(|item| item.path.clone())
        .collect();
    assert!(paths.contains(&tmp.join(".hidden.mkv")));
    assert!(paths.contains(&tmp.join(".hidden-dir/inside.mkv")));
    assert!(!paths.contains(&tmp.join("movie-extra.mkv")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn duration_str_hms_millis() {
    assert_eq!(duration_str(0), "0:00:00.000");
    assert_eq!(duration_str(3_661_234), "1:01:01.234");
}

#[test]
fn nfo_year_becomes_ten_char_date() {
    assert_eq!(
        nfo_date_from_text("<movie><year>1999</year></movie>").as_deref(),
        Some("1999-01-01")
    );
}

const EPISODE_NFO: &str = r#"<episodedetails>
  <showtitle>The Show</showtitle>
  <title>Pilot</title>
  <outline>The safe outline</outline>
  <plot>The plot text</plot>
  <genre>Drama</genre>
  <genre>Crime</genre>
  <director>Jane Doe</director>
  <studio>Network</studio>
  <season>1</season>
  <episode>2</episode>
  <premiered>2020-05-01</premiered>
</episodedetails>"#;

#[test]
fn nfo_title_plot_show_season() {
    let parsed = parse_nfo_text(EPISODE_NFO);
    assert_eq!(parsed.title.as_deref(), Some("The Show - Pilot"));
    assert_eq!(parsed.showtitle.as_deref(), Some("The Show"));
    assert_eq!(parsed.episode_title.as_deref(), Some("Pilot"));
    assert_eq!(
        episode_display_title("The Show - Pilot", Some("The Show")),
        "Pilot"
    );
    assert_eq!(parsed.about.as_deref(), Some("The safe outline"));
    assert_eq!(parsed.plot.as_deref(), Some("The plot text"));
    assert_eq!(parsed.genre.as_deref(), Some("Drama / Crime"));
    assert_eq!(parsed.creator.as_deref(), Some("Jane Doe"));
    assert!(
        parsed.artist.as_deref() == Some("Network") || parsed.artist.as_deref() == Some("The Show"),
        "artist={:?}",
        parsed.artist
    );
    assert_eq!(parsed.disc, Some(1));
    assert_eq!(parsed.track, Some(2));
    assert!(
        parsed
            .date
            .as_deref()
            .is_some_and(|d| d.starts_with("2020-05-01")),
        "date={:?}",
        parsed.date
    );

    let credits = parse_nfo_text("<credits>Pat Lee</credits>");
    assert_eq!(credits.creator.as_deref(), Some("Pat Lee"));
    let studio = parse_nfo_text("<studio>Network</studio>");
    assert_eq!(studio.creator.as_deref(), Some("Network"));
    assert_eq!(studio.artist.as_deref(), Some("Network"));
    let director_wins = parse_nfo_text(
        "<director>Jane Doe</director><credits>Pat Lee</credits><studio>Network</studio>",
    );
    assert_eq!(director_wins.creator.as_deref(), Some("Jane Doe"));

    assert!(nfo_too_large(64 * 1024 + 1));
    assert!(!nfo_too_large(64 * 1024));

    let tmp = TempPath::new("nfo-ep");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("show")).unwrap();
    write_fake_mkv(&tmp.join("show/S01E01.mkv"), 64);
    std::fs::write(tmp.join("show/S01E01.nfo"), EPISODE_NFO).unwrap();
    let huge = "x".repeat(64 * 1024 + 1);
    std::fs::write(
        tmp.join("show/huge.nfo"),
        format!("<title>TooBig</title>{huge}"),
    )
    .unwrap();
    let huge_meta = nfo_for_file(&tmp.join("show/huge.mkv"), std::slice::from_ref(&tmp));
    assert!(huge_meta.title.is_none(), "skip >64KiB: {huge_meta:?}");

    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    })
    .unwrap();
    let ep = cat
        .items
        .values()
        .find(|i| i.title == "The Show - Pilot")
        .expect("episode title from nfo");
    assert!(
        ep.plot
            .as_deref()
            .is_some_and(|c| c.contains("The plot text")),
        "plot={:?}",
        ep.plot
    );
    assert_eq!(ep.about.as_deref(), Some("The safe outline"));
    assert_eq!(ep.genre.as_deref(), Some("Drama / Crime"));
    assert_eq!(ep.creator.as_deref(), Some("Jane Doe"));
    assert!(
        ep.artist.as_deref() == Some("Network") || ep.artist.as_deref() == Some("The Show"),
        "artist={:?}",
        ep.artist
    );
    assert_eq!(ep.disc, Some(1));
    assert_eq!(ep.track, Some(2));
    assert!(ep.date.starts_with("2020-05-01"), "date={}", ep.date);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn tvshow_nfo_inherited_by_episode() {
    let tmp = TempPath::new("nfo-tv");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("show")).unwrap();
    std::fs::write(
        tmp.join("show/tvshow.nfo"),
        r#"<tvshow>
  <title>The Show</title>
  <outline>Show outline</outline>
  <plot>Show plot</plot>
  <genre>Drama</genre>
  <studio>Network</studio>
</tvshow>"#,
    )
    .unwrap();
    write_fake_mkv(&tmp.join("show/S01E01.mkv"), 64);
    std::fs::write(
        tmp.join("show/S01E01.nfo"),
        "<episodedetails><title>Pilot</title></episodedetails>\n",
    )
    .unwrap();
    write_fake_mkv(&tmp.join("show/S01E02.mkv"), 64);
    std::fs::write(
        tmp.join("show/S01E02.nfo"),
        "<episodedetails><title>Second</title><plot>Own plot</plot></episodedetails>\n",
    )
    .unwrap();

    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    })
    .unwrap();
    let ep1 = cat
        .items
        .values()
        .find(|i| i.path.ends_with("S01E01.mkv") && i.ref_id.is_none())
        .expect("S01E01");
    assert_eq!(ep1.title, "The Show - Pilot");
    assert_eq!(ep1.about.as_deref(), Some("Show outline"));
    assert_eq!(ep1.plot.as_deref(), Some("Show plot"));
    assert_eq!(ep1.genre.as_deref(), Some("Drama"));
    let ep2 = cat
        .items
        .values()
        .find(|i| i.path.ends_with("S01E02.mkv") && i.ref_id.is_none())
        .expect("S01E02");
    assert_eq!(ep2.title, "The Show - Second");
    assert_eq!(ep2.about.as_deref(), Some("Show outline"));
    assert_eq!(ep2.plot.as_deref(), Some("Own plot"));
    assert_eq!(ep2.genre.as_deref(), Some("Drama"));
    assert_eq!(ep1.album.as_deref(), Some("The Show"));
    assert_eq!(ep2.album.as_deref(), Some("The Show"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn series_and_genre_trees_from_nfo() {
    let tmp = TempPath::new("series");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("The Show")).unwrap();
    std::fs::write(
        tmp.join("The Show/tvshow.nfo"),
        r#"<tvshow><title>The Show</title><genre>Drama</genre><genre>Crime</genre></tvshow>"#,
    )
    .unwrap();
    write_fake_mkv(&tmp.join("The Show/S01E01.mkv"), 64);
    std::fs::write(
            tmp.join("The Show/S01E01.nfo"),
            "<episodedetails><title>Pilot</title><season>1</season><episode>1</episode></episodedetails>\n",
        )
        .unwrap();
    write_fake_mkv(&tmp.join("The Show/S01E02.mkv"), 64);
    std::fs::write(
            tmp.join("The Show/S01E02.nfo"),
            "<episodedetails><title>Second</title><season>1</season><episode>2</episode></episodedetails>\n",
        )
        .unwrap();
    write_fake_mkv(&tmp.join("The Show/S02E01.mkv"), 64);
    std::fs::write(
            tmp.join("The Show/S02E01.nfo"),
            "<episodedetails><title>Return</title><season>2</season><episode>1</episode></episodedetails>\n",
        )
        .unwrap();
    std::fs::create_dir_all(tmp.join("movies")).unwrap();
    write_fake_mkv(&tmp.join("movies/film.mkv"), 64);
    std::fs::write(
        tmp.join("movies/film.nfo"),
        "<movie><title>Standalone</title><genre>Action</genre></movie>\n",
    )
    .unwrap();

    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    })
    .unwrap();
    assert!(cat.containers.contains_key(VIDEO_SERIES_ID));
    assert!(cat.containers.contains_key(VIDEO_GENRE_ID));
    let series = cat.children_of(VIDEO_SERIES_ID).expect("series");
    let shows: Vec<_> = series
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Container(x) => Some(x.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(shows, ["The Show"], "{shows:?}");
    let show = series
        .iter()
        .find_map(|c| match c {
            CatalogChild::Container(x) if x.title == "The Show" => Some(x),
            _ => None,
        })
        .expect("show container");
    let seasons = cat.children_of(&show.object_id).expect("seasons");
    let season_titles: Vec<_> = seasons
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Container(x) => Some(x.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(season_titles, ["Season 1", "Season 2"], "{season_titles:?}");
    let s1 = seasons
        .iter()
        .find_map(|c| match c {
            CatalogChild::Container(x) if x.title == "Season 1" => Some(x),
            _ => None,
        })
        .unwrap();
    let eps = cat.children_of(&s1.object_id).expect("s1 eps");
    let ep_titles: Vec<_> = eps
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Item(i) => Some(i.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ep_titles, ["Pilot", "Second"], "{ep_titles:?}");
    assert!(
        eps.iter().all(|c| match c {
            CatalogChild::Item(i) => i.ref_id.is_some(),
            _ => true,
        }),
        "series items must be REF_ID aliases"
    );
    let genres = cat.children_of(VIDEO_GENRE_ID).expect("genres");
    let genre_names: Vec<_> = genres
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Container(x) => Some(x.title.as_str()),
            _ => None,
        })
        .collect();
    assert!(genre_names.contains(&"Drama"), "{genre_names:?}");
    assert!(genre_names.contains(&"Crime"), "{genre_names:?}");
    assert!(genre_names.contains(&"Action"), "{genre_names:?}");
    let action = genres
        .iter()
        .find_map(|c| match c {
            CatalogChild::Container(x) if x.title == "Action" => Some(x),
            _ => None,
        })
        .unwrap();
    let action_items = cat.children_of(&action.object_id).expect("action items");
    assert!(
        action_items.iter().any(|c| match c {
            CatalogChild::Item(i) => i.title.contains("Standalone"),
            _ => false,
        }),
        "{action_items:?}"
    );
    let show_ids: Vec<_> = series
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Container(x) => Some(x.object_id.as_str()),
            _ => None,
        })
        .collect();
    for id in show_ids {
        let kids = cat.children_of(id).unwrap_or_default();
        assert!(
            !kids.iter().any(|c| match c {
                CatalogChild::Item(i) => i.title.contains("Standalone"),
                _ => false,
            }),
            "movie must not appear under Series"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn bookmark_survives_reopen() {
    let tmp = TempPath::new("bookmark");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let dbp = tmp.join("files.db");
    let detail_id;
    {
        let db = LibraryDb::open(&dbp).unwrap();
        detail_id = db
            .insert_detail(NewDetail {
                path: "/media/bookmark.mp4",
                size: 1,
                timestamp: 1,
                title: "bookmark",
                date: "",
                mime: "video/mp4",
                device: 1,
                inode: 1,
                dlna_pn: None,
            })
            .unwrap();
        db.set_bookmark(detail_id, 120).unwrap();
        db.set_watch_count(detail_id, 3).unwrap();
        assert_eq!(db.get_bookmark(detail_id).unwrap(), Some((120, 3)));
    }
    let db = LibraryDb::open(&dbp).unwrap();
    assert_eq!(
        db.get_bookmark(detail_id).unwrap(),
        Some((120, 3)),
        "BOOKMARKS must survive LibraryDb::open"
    );
    assert_eq!(db.get_bookmark(7).unwrap(), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn bookmark_retention_uses_last_update_and_zero_is_indefinite() {
    let db = LibraryDb::open_memory().unwrap();
    let insert = |path: &'static str, inode| {
        db.insert_detail(NewDetail {
            path,
            size: 1,
            timestamp: 1,
            title: path,
            date: "",
            mime: "video/mp4",
            device: 1,
            inode,
            dlna_pn: None,
        })
        .unwrap()
    };
    let old = insert("/media/old.mp4", 1);
    let fresh = insert("/media/fresh.mp4", 2);
    db.update_bookmark(old, Some(120), Some(1)).unwrap();
    db.update_bookmark(fresh, Some(240), Some(2)).unwrap();

    let now = 20_000_000_i64;
    db.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![now - 91 * 86_400, old],
        )
        .unwrap();
    db.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![now - 90 * 86_400, fresh],
        )
        .unwrap();

    assert_eq!(db.prune_expired_bookmarks(0, now).unwrap(), 0);
    assert_eq!(db.get_bookmark(old).unwrap(), Some((120, 1)));
    assert_eq!(db.prune_expired_bookmarks(90, now).unwrap(), 1);
    assert_eq!(db.get_bookmark(old).unwrap(), None);
    assert_eq!(
        db.get_bookmark(fresh).unwrap(),
        Some((240, 2)),
        "state exactly on the retention boundary remains valid"
    );
}

#[test]
fn full_reconciliation_prunes_expired_bookmarks_and_republishes_catalog() {
    let tmp = TempPath::new("bookmark-reconcile");
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    write_fake_mkv(&media.join("movie.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media],
        db_path: Some(tmp.join("files.db")),
        bookmark_retention_days: 90,
        types: MediaTypes::video_only(),
        ..ScanConfig::default()
    };
    let initial = scan(&cfg).unwrap();
    let item = initial.items.values().next().unwrap().clone();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    db.set_bookmark(item.detail_id, 120).unwrap();
    db.set_watch_count(item.detail_id, 2).unwrap();
    let mut stale = initial.clone();
    db.hydrate_catalog_bookmarks(&mut stale).unwrap();
    assert_eq!(
        stale
            .get_item_by_detail(item.detail_id)
            .map(|item| (item.bookmark_sec, item.watch_count)),
        Some((120, 2))
    );
    db.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![unix_now_seconds() - 91 * 86_400, item.detail_id],
        )
        .unwrap();
    drop(db);

    let (catalog, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let catalog = catalog.expect("bookmark expiry must republish the catalog");
    assert_eq!(
        catalog
            .get_item_by_detail(item.detail_id)
            .unwrap()
            .bookmark_sec,
        0
    );
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    assert_eq!(db.get_bookmark(item.detail_id).unwrap(), None);
    db.hydrate_catalog_bookmarks(&mut stale).unwrap();
    assert_eq!(
        stale
            .get_item_by_detail(item.detail_id)
            .map(|item| (item.bookmark_sec, item.watch_count)),
        Some((0, 0)),
        "publication hydration must preserve scanner retention deletions"
    );
}

#[test]
fn catalog_patch_hydration_does_not_resurrect_retained_bookmark() {
    let tmp = TempPath::new("bookmark-patch-hydration");
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    write_fake_mkv(&media.join("movie.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..ScanConfig::default()
    };
    let mut published = scan(&cfg).unwrap();
    let detail_id = published.items.values().next().unwrap().detail_id;
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    db.begin_catalog_change_capture().unwrap();
    db.update_bookmark(detail_id, Some(120), Some(2)).unwrap();
    let stale_title = "stale patch title";
    db.update_detail_title(detail_id, stale_title).unwrap();
    let mut stale_patch = db.load_catalog_patch().unwrap();
    assert!(!stale_patch.items.is_empty());
    assert!(stale_patch
        .items
        .iter()
        .any(|item| item.title == stale_title));

    let now = unix_now_seconds();
    db.connection()
        .execute(
            "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
            rusqlite::params![now - 91 * 86_400, detail_id],
        )
        .unwrap();
    assert_eq!(db.prune_expired_bookmarks(90, now).unwrap(), 1);
    db.hydrate_catalog_patch_bookmarks(&mut stale_patch)
        .unwrap();
    published.apply_patch(stale_patch);
    let item = published.get_item_by_detail(detail_id).unwrap();
    assert_eq!(item.title, stale_title);
    assert_eq!((item.bookmark_sec, item.watch_count), (0, 0));
}

#[test]
fn bookmark_hydration_error_leaves_catalog_and_patch_unchanged() {
    let tmp = TempPath::new("bookmark-hydration-error");
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    write_fake_mkv(&media.join("movie.mkv"), 64);
    let cfg = ScanConfig {
        media_dirs: vec![media],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..ScanConfig::default()
    };
    let mut catalog = scan(&cfg).unwrap();
    let detail_id = catalog.items.values().next().unwrap().detail_id;
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    db.begin_catalog_change_capture().unwrap();
    db.update_bookmark(detail_id, Some(120), Some(2)).unwrap();
    db.update_detail_title(detail_id, "changed title").unwrap();
    let mut patch = db.load_catalog_patch().unwrap();
    assert!(!patch.items.is_empty());
    for item in catalog.items.values_mut() {
        if item.detail_id == detail_id {
            item.bookmark_sec = 777;
            item.watch_count = 8;
        }
    }
    let catalog_before = format!("{catalog:#?}");
    let patch_before = format!("{patch:#?}");
    db.connection()
        .execute(
            "UPDATE BOOKMARKS SET SEC = 'not-an-integer' WHERE ID = ?1",
            [detail_id],
        )
        .unwrap();

    assert!(db.hydrate_catalog_bookmarks(&mut catalog).is_err());
    assert_eq!(format!("{catalog:#?}"), catalog_before);
    assert!(db.hydrate_catalog_patch_bookmarks(&mut patch).is_err());
    assert_eq!(format!("{patch:#?}"), patch_before);
}

#[test]
fn scan_skips_junk_sample_exclude_and_reads_nfo_captions() {
    let tmp = TempPath::new("scan");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    std::fs::create_dir_all(tmp.join("sample")).unwrap();
    std::fs::create_dir_all(tmp.join("@eaDir")).unwrap();
    std::fs::create_dir_all(tmp.join("exclude_me")).unwrap();
    write_fake_mkv(&tmp.join("video/movie.mkv"), 64);
    std::fs::write(tmp.join("video/movie.nfo"), "<year>1999</year>").unwrap();
    std::fs::write(tmp.join("video/movie.srt"), b"1").unwrap();
    std::fs::write(tmp.join("video/movie.en.srt"), b"2").unwrap();
    std::fs::write(tmp.join("sample/skip.mkv"), b"x").unwrap();
    std::fs::write(tmp.join("@eaDir/junk.mkv"), b"x").unwrap();
    std::fs::write(tmp.join("exclude_me/secret.mkv"), b"x").unwrap();
    std::fs::write(tmp.join("unfinished.mkv.part"), b"x").unwrap();
    std::fs::write(
        tmp.join("video/dvp7.probe.toml"),
        "hdr = \"dv-p7\"\naudio = \"truehd\"\n",
    )
    .unwrap();
    write_fake_mkv(&tmp.join("video/dvp7.mkv"), 64);
    std::fs::write(
        tmp.join("video/not-video.mkv"),
        b"this is a text file not a video",
    )
    .unwrap();
    std::fs::write(tmp.join("video/notes.txt"), b"ignore").unwrap();

    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        exclude_dirs: vec!["exclude_me".into()],
        exclude_files: vec![],
        ..Default::default()
    })
    .unwrap();
    assert!(
        !cat.items
            .values()
            .any(|i| i.title == "not-video" || i.title == "notes"),
        "text must not be indexed as video"
    );
    let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
    assert!(titles.contains(&"movie"));
    assert!(titles.contains(&"dvp7"));
    assert!(!titles
        .iter()
        .any(|t| *t == "skip" || *t == "secret" || *t == "junk"));
    let movie = cat
        .items
        .values()
        .find(|i| i.title == "movie" && i.parent_id != VIDEO_ALL_ID)
        .unwrap();
    assert_eq!(movie.date, "1999-01-01");
    assert!(movie.captions.len() >= 2);
    let dvp7 = cat.items.values().find(|i| i.title == "dvp7").unwrap();
    assert_eq!(dvp7.probe.hdr, "dv-p7");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn inode_reuse_for_hardlink_alias() {
    let tmp = TempPath::new("inode");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let a = tmp.join("video/orig.mkv");
    write_fake_mkv(&a, 64);
    let b = tmp.join("video/alias.mkv");
    let _ = std::fs::remove_file(&b);
    std::fs::hard_link(&a, &b).unwrap();
    let dbp = tmp.join("files.db");
    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(dbp.clone()),
        ..Default::default()
    })
    .unwrap();
    let orig = cat
        .items
        .values()
        .find(|i| i.path.ends_with("orig.mkv"))
        .expect("orig.mkv");
    let alias = cat
        .items
        .values()
        .find(|i| i.path.ends_with("alias.mkv"))
        .expect("alias.mkv");
    let db = LibraryDb::open(&dbp).unwrap();
    let o = db
        .find_detail_by_path(&orig.path.to_string_lossy())
        .unwrap()
        .unwrap();
    let arow = db
        .find_detail_by_path(&alias.path.to_string_lossy())
        .unwrap()
        .unwrap();
    // rustyDLNA: one DETAILS row per path; same DEVICE+INODE.
    assert_ne!(o.id, arow.id);
    let all_video: Vec<_> = cat
        .items
        .values()
        .filter(|i| i.parent_id == VIDEO_ALL_ID)
        .collect();
    assert_eq!(all_video.len(), 1, "All Video must not list hardlink twice");
    assert_eq!(orig.date, alias.date, "alias must clone original DATE");
    assert_eq!(orig.mime, alias.mime, "alias must clone original MIME");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn bounded_scan_jobs_use_parallelism_without_exceeding_limit() {
    let jobs: Vec<usize> = (0..24).collect();
    let active = std::sync::atomic::AtomicUsize::new(0);
    let peak = std::sync::atomic::AtomicUsize::new(0);
    let results = run_bounded_jobs(&jobs, 4, &CancellationToken::default(), |job| {
        let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(5));
        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        job * 2
    });
    assert_eq!(
        results,
        jobs.iter().map(|job| Some(job * 2)).collect::<Vec<_>>()
    );
    assert!(peak.load(std::sync::atomic::Ordering::SeqCst) > 1);
    assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 4);
}

#[test]
fn helper_gate_is_fifo_and_never_exceeds_the_global_limit() {
    let gate = std::sync::Arc::new(HelperGate::new(1, 8));
    let initial = gate.try_acquire().unwrap();
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for id in 0..4 {
        let worker_gate = std::sync::Arc::clone(&gate);
        let order = std::sync::Arc::clone(&order);
        workers.push(std::thread::spawn(move || {
            let _permit = worker_gate
                .acquire_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            order.lock().unwrap().push(id);
        }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while gate.metrics().queued < id + 1 {
            assert!(std::time::Instant::now() < deadline, "waiter did not queue");
            std::thread::yield_now();
        }
    }
    drop(initial);
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3]);
    let metrics = gate.metrics();
    assert_eq!(metrics.active, 0);
    assert_eq!(metrics.queued, 0);
    assert_eq!(metrics.admitted_total, 5);
    assert_eq!(metrics.saturated_total, 4);
    assert_eq!(metrics.queued_total, 4);
    assert_eq!(metrics.wait_duration_ms_buckets.iter().sum::<u64>(), 4);
}

#[test]
fn helper_gate_bounds_queue_and_records_rejection_and_timeout() {
    let gate = std::sync::Arc::new(HelperGate::new(1, 1));
    let initial = gate.try_acquire().unwrap();
    let waiting_gate = std::sync::Arc::clone(&gate);
    let waiter = std::thread::spawn(move || {
        waiting_gate.acquire_timeout(std::time::Duration::from_millis(50))
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while gate.metrics().queued != 1 {
        assert!(std::time::Instant::now() < deadline, "waiter did not queue");
        std::thread::yield_now();
    }
    assert!(matches!(
        gate.acquire_timeout(std::time::Duration::from_secs(1)),
        Err(HelperAdmissionError::Rejected)
    ));
    assert!(matches!(
        waiter.join().unwrap(),
        Err(HelperAdmissionError::TimedOut)
    ));
    drop(initial);
    let metrics = gate.metrics();
    assert_eq!(metrics.active, 0);
    assert_eq!(metrics.queued, 0);
    assert_eq!(metrics.saturated_total, 2);
    assert_eq!(metrics.queued_total, 1);
    assert_eq!(metrics.rejected_total, 1);
    assert_eq!(metrics.timed_out_total, 1);
    assert_eq!(metrics.wait_duration_ms_buckets.iter().sum::<u64>(), 1);
}

#[test]
fn helper_gate_wait_is_cancelled_without_waiting_for_its_deadline() {
    let gate = std::sync::Arc::new(HelperGate::new(1, 1));
    let initial = gate.try_acquire().unwrap();
    let cancellation = CancellationToken::default();
    let waiter_gate = std::sync::Arc::clone(&gate);
    let waiter_cancellation = cancellation.clone();
    let started = std::time::Instant::now();
    let waiter = std::thread::spawn(move || {
        waiter_gate
            .acquire_timeout_cancelled(std::time::Duration::from_secs(30), &waiter_cancellation)
    });
    while gate.metrics().queued != 1 {
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::thread::yield_now();
    }
    cancellation.cancel();
    assert!(matches!(
        waiter.join().unwrap(),
        Err(HelperAdmissionError::Cancelled)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(gate.metrics().queued, 0);
    drop(initial);
}

#[cfg(unix)]
#[test]
fn physical_preparation_prefers_real_path_and_groups_directory_symlink() {
    let tmp = TempPath::new("prepare-alias");
    std::fs::create_dir_all(tmp.join("zz-real")).unwrap();
    let direct = tmp.join("zz-real/movie.mkv");
    write_fake_mkv(&direct, 64);
    let alias_dir = tmp.join("00-alias");
    std::os::unix::fs::symlink(tmp.join("zz-real"), &alias_dir).unwrap();
    let alias = alias_dir.join("movie.mkv");
    let alias_meta = std::fs::metadata(&alias).unwrap();
    let direct_meta = std::fs::metadata(&direct).unwrap();
    let pending = vec![
        PendingFile {
            path: alias.clone(),
            folder_id: "alias".into(),
            physical: PhysicalFileKey::new(&alias, &alias_meta).unwrap(),
            probe_sidecar_fingerprint: NO_PROBE_SIDECAR_FINGERPRINT.into(),
            album_art_sidecar: None,
        },
        PendingFile {
            path: direct.clone(),
            folder_id: "direct".into(),
            physical: PhysicalFileKey::new(&direct, &direct_meta).unwrap(),
            probe_sidecar_fingerprint: NO_PROBE_SIDECAR_FINGERPRINT.into(),
            album_art_sidecar: None,
        },
    ];
    let db = LibraryDb::open_memory().unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        thumbnails: false,
        scan_workers: 4,
        ..Default::default()
    };
    let prepared = prepare_pending_files(&db, &cfg, &pending).unwrap();
    assert_eq!(prepared.by_physical_and_sidecar.len(), 1);
    assert_eq!(prepared.worker_indices, HashSet::from([1]));
    let direct_identity = source_image_identity(&direct).unwrap();
    assert_eq!(direct_identity.len(), 64);
    assert_eq!(
        direct_identity,
        source_image_identity(&alias).unwrap(),
        "cache identity must follow the physical file, not its alias path"
    );
}

#[test]
fn walker_streams_preparation_in_bounded_ordered_batches() {
    let tmp = TempPath::new("bounded-preparation");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut paths = Vec::new();
    for index in 0..5 {
        let path = tmp.join(format!("movie-{index}.mkv"));
        let mut data = vec![0u8; 32];
        data[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
        std::fs::write(&path, data).unwrap();
        paths.push(path);
    }

    let db = LibraryDb::open_memory().unwrap();
    db.seed_virtual_containers().unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        thumbnails: false,
        scan_workers: 2,
        ..Default::default()
    };
    let mut walker = DbWalker {
        db: &db,
        cfg: &cfg,
        walk_stack: HashMap::new(),
        rebuild: true,
        indexed: 0,
        pending: Vec::new(),
        pending_limit: 2,
        preparation_batches: 0,
        peak_pending: 0,
        physical_artwork_inventories: HashMap::new(),
    };
    walker.walk(&tmp, BROWSEDIR_ID, "media").unwrap();
    walker.index_pending().unwrap();

    assert_eq!(walker.preparation_batches, 3);
    assert_eq!(walker.peak_pending, 2);
    assert!(walker.pending.is_empty());
    assert_eq!(walker.indexed, paths.len());
    let ids: Vec<i64> = paths
        .iter()
        .map(|path| {
            db.find_detail_by_path(&path_to_db(path))
                .unwrap()
                .unwrap()
                .id
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[cfg(unix)]
#[test]
fn directory_symlink_alias_reuses_one_generated_thumbnail() {
    let tmp = TempPath::new("thumb-alias");
    let real_dir = tmp.join("zz-real");
    std::fs::create_dir_all(&real_dir).unwrap();
    let direct = real_dir.join("movie.mp4");
    let generated = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=64x64:rate=2",
            "-threads",
            "1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&direct)
        .stdin(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !generated {
        eprintln!("skip alias thumbnail reuse (ffmpeg fixture unavailable)");
        return;
    }
    let alias_dir = tmp.join("00-alias");
    std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
    let alias = alias_dir.join("movie.mp4");
    let db_path = tmp.join("cache/files.db");
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: true,
        scan_workers: 4,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let db = LibraryDb::open(&db_path).unwrap();
    let alias_detail = db
        .find_detail_by_path(&path_to_db(&alias))
        .unwrap()
        .expect("alias detail");
    let direct_detail = db
        .find_detail_by_path(&path_to_db(&direct))
        .unwrap()
        .expect("direct detail");
    let alias_art = db.detail_album_art(alias_detail.id).unwrap();
    let direct_art = db.detail_album_art(direct_detail.id).unwrap();
    assert!(alias_art > 0);
    assert_eq!(alias_art, direct_art, "aliases must share one artwork row");
    assert!(
        db.details_missing_stream_meta().unwrap().is_empty(),
        "the initial scan must persist its prepared probe for every alias"
    );
    let (_, unchanged) = monitor(&cfg).unwrap();
    assert_eq!(unchanged, ScanDelta::default());
    let thumbnails = std::fs::read_dir(tmp.join("cache/art"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("thumb-"))
        .count();
    assert_eq!(thumbnails, 1, "ffmpeg must generate one physical thumbnail");
}

/// 1×1 JPEG (SOI + JFIF + EOI). Real enough for sidecar + HTTP magic checks.
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

fn write_solid_jpeg(path: &Path, width: u32, height: u32) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("color=c=red:s={width}x{height}:d=0.1"))
        .args(["-frames:v", "1"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn jpeg_magic_and_art_name_png() {
    assert!(is_jpeg_bytes(TINY_JPEG));
    assert!(!is_jpeg_bytes(b"\x89PNG"));
    assert!(is_album_art_name("clip-poster.png"));
    assert!(is_album_art_name("clip-fanart.png"));
    assert!(is_album_art_name("poster.png"));
    assert!(is_album_art_name("Poster.jpg"));
    assert!(!is_album_art_name("clip.mkv"));

    let repeated = ScanConfig {
        album_art_names: vec!["{stem}-cover-%s.jpg".into()],
        ..Default::default()
    };
    assert!(is_album_art_name_for_config(
        "movie-cover-movie.jpg",
        &repeated
    ));
    assert!(!is_album_art_name_for_config(
        "movie-cover-other.jpg",
        &repeated
    ));
    assert!(!is_album_art_name_for_config("unrelated.jpg", &repeated));
}

#[test]
fn art_sidecar_indexed_and_cloned() {
    let tmp = TempPath::new("art");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    write_fake_mkv(&tmp.join("clip.mkv"), 64);
    std::fs::write(tmp.join("clip-poster.jpg"), TINY_JPEG).unwrap();
    let dbp = tmp.join("files.db");
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(dbp.clone()),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    let video = cat
        .items
        .values()
        .find(|i| i.path.ends_with("clip.mkv"))
        .expect("clip indexed");
    assert!(video.album_art > 0, "sidecar must set ALBUM_ART: {video:?}");
    assert!(
        !cat.items
            .values()
            .any(|i| i.title.contains("poster") || i.title == "clip-poster"),
        "art files stay skipped"
    );
    assert!(cat.album_art_paths.contains_key(&video.album_art));

    let alias = tmp.join("alias.mkv");
    let _ = std::fs::remove_file(&alias);
    std::fs::hard_link(tmp.join("clip.mkv"), &alias).unwrap();
    let (cat2, _) = rescan(&cfg, &cat).unwrap();
    let clip2 = cat2
        .items
        .values()
        .find(|i| i.path.ends_with("clip.mkv"))
        .expect("clip after clone");
    let alias_item = cat2
        .items
        .values()
        .find(|i| i.path.ends_with("alias.mkv"))
        .expect("hardlink alias");
    assert_eq!(
        alias_item.album_art, clip2.album_art,
        "clone must share ALBUM_ART id"
    );
    assert!(alias_item.album_art > 0);

    let poster = tmp.join("clip-poster.jpg");
    let _ = std::fs::write(&poster, TINY_JPEG);
    let (cat3, _) = monitor_dirty(&cfg, &[poster]).unwrap();
    let cat3 = cat3.unwrap_or(cat2);
    let ids: Vec<i64> = cat3
        .items
        .values()
        .filter(|i| i.path.ends_with("clip.mkv") || i.path.ends_with("alias.mkv"))
        .map(|i| i.album_art)
        .collect();
    assert!(ids.len() >= 2, "both aliases still listed");
    assert!(ids.iter().all(|id| *id == ids[0] && *id > 0));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn sidecar_write_replace_and_delete_recompute_one_catalog_generation() {
    let tmp = TempPath::new("sidecar-lifecycle");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let media = tmp.join("movie.mkv");
    let nfo = tmp.join("movie.nfo");
    let caption = tmp.join("movie.en.srt");
    let unrelated_caption = tmp.join("movie2.srt");
    let poster = tmp.join("movie-poster.jpg");
    write_fake_mkv(&media, 64);
    std::fs::write(
        &nfo,
        "<movie><title>Sidecar Title</title><outline>Safe sidecar outline</outline><plot>Spoiler sidecar plot</plot><genre>Drama</genre></movie>",
    )
    .unwrap();
    std::fs::write(&caption, "first subtitle").unwrap();
    std::fs::write(&unrelated_caption, "must not attach").unwrap();
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };

    let initial = scan(&cfg).unwrap();
    let item = initial
        .items
        .values()
        .find(|item| item.path == media && item.ref_id.is_none())
        .unwrap();
    let detail_id = item.detail_id;
    let sidecar_art_id = item.album_art;
    assert_eq!(item.title, "Sidecar Title");
    assert_eq!(item.about.as_deref(), Some("Safe sidecar outline"));
    assert_eq!(item.plot.as_deref(), Some("Spoiler sidecar plot"));
    assert_eq!(item.captions.len(), 1, "movie2.srt must not attach");
    assert!(sidecar_art_id > 0);

    std::fs::write(&caption, "replacement subtitle bytes").unwrap();
    let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&caption)).unwrap();
    assert!(catalog.is_some());
    assert_eq!(delta.changed, 1, "one settled sidecar burst is one change");

    // A replacement at the same path still invalidates clients even
    // though its ALBUM_ART row/path is stable.
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let (_, delta) = monitor_dirty(&cfg, std::slice::from_ref(&poster)).unwrap();
    assert_eq!(delta.changed, 1);

    // Malformed sidecar parsing is transactional: the last published DB
    // generation remains fully readable and unchanged.
    std::fs::write(&nfo, "<movie><title>broken</movie>").unwrap();
    assert!(monitor_dirty(&cfg, std::slice::from_ref(&nfo)).is_err());
    let retained = open_library_db(cfg.db_path.as_ref().unwrap())
        .unwrap()
        .load_catalog()
        .unwrap();
    assert_eq!(retained.by_detail[&detail_id], item.object_id);
    assert_eq!(retained.items[&item.object_id].title, "Sidecar Title");

    std::fs::remove_file(&nfo).unwrap();
    std::fs::remove_file(&caption).unwrap();
    std::fs::remove_file(&poster).unwrap();
    let (catalog, delta) =
        monitor_dirty(&cfg, &[nfo.clone(), caption.clone(), poster.clone()]).unwrap();
    assert_eq!(delta.changed, 1, "a multi-sidecar burst bumps once");
    let catalog = catalog.unwrap();
    let item = &catalog.items[&catalog.by_detail[&detail_id]];
    assert_eq!(item.title, "movie", "NFO deletion restores filename title");
    assert_eq!(item.about, None, "NFO deletion removes the old outline");
    assert_eq!(item.plot, None, "NFO deletion removes the old plot");
    assert!(item.captions.is_empty(), "caption deletion must propagate");
    assert_ne!(
        item.album_art, sidecar_art_id,
        "deleted art cannot stay selected"
    );
    assert!(
        !catalog.items.values().any(|candidate| {
            candidate.detail_id == detail_id && candidate.parent_id.starts_with(VIDEO_GENRE_ID)
        }),
        "NFO deletion must remove stale genre aliases"
    );
    let db = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    assert!(db.album_art_path(sidecar_art_id).unwrap().is_none());

    // A periodic reconcile (no inotify paths) reaches the same desired
    // state for sidecar additions and deletions.
    std::fs::write(
        &nfo,
        "<movie><title>Periodic Title</title><outline>Periodic outline</outline><plot>Periodic plot</plot></movie>",
    )
    .unwrap();
    std::fs::write(&caption, "periodic subtitle").unwrap();
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let (catalog, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let catalog = catalog.unwrap();
    let item = &catalog.items[&catalog.by_detail[&detail_id]];
    assert_eq!(item.title, "Periodic Title");
    assert_eq!(item.about.as_deref(), Some("Periodic outline"));
    assert_eq!(item.plot.as_deref(), Some("Periodic plot"));
    assert_eq!(item.captions.len(), 1);
    assert!(item.album_art > 0);

    // Removing only the description tags while retaining the NFO must not
    // leave either value behind after a periodic reconciliation.
    std::fs::write(&nfo, "<movie><title>Periodic Title</title></movie>").unwrap();
    let (catalog, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let catalog = catalog.unwrap();
    let item = &catalog.items[&catalog.by_detail[&detail_id]];
    assert_eq!(item.about, None);
    assert_eq!(item.plot, None);

    std::fs::remove_file(&nfo).unwrap();
    std::fs::remove_file(&caption).unwrap();
    std::fs::remove_file(&poster).unwrap();
    let (catalog, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let catalog = catalog.unwrap();
    let item = &catalog.items[&catalog.by_detail[&detail_id]];
    assert_eq!(item.title, "movie");
    assert_eq!(item.about, None);
    assert_eq!(item.plot, None);
    assert!(item.captions.is_empty());
    assert_eq!(item.album_art, 0);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn art_embedded_and_thumbnail_fallback() {
    let tmp = TempPath::new("embed-art");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let poster = tmp.join("cover.jpg");
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let embedded = tmp.join("withcover.mp4");
    let mk = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=2",
            "-i",
        ])
        .arg(&poster)
        .args([
            "-map",
            "0",
            "-map",
            "1",
            "-c:v:0",
            "libx264",
            "-pix_fmt:v:0",
            "yuv420p",
            "-c:v:1",
            "mjpeg",
            "-disposition:1",
            "attached_pic",
        ])
        .arg(&embedded)
        .stdin(std::process::Stdio::null())
        .status();
    if !mk.map(|s| s.success()).unwrap_or(false) {
        eprintln!("skip embedded art (could not mux attached_pic)");
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    assert!(
        attached_pic_stream(&embedded).is_some(),
        "fixture must expose attached_pic"
    );
    let bare = tmp.join("bare.mkv");
    write_fake_mkv(&bare, 64);
    let dbp = tmp.join("files.db");
    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(dbp),
        types: MediaTypes::video_only(),
        ..Default::default()
    })
    .unwrap();
    let cover = cat
        .items
        .values()
        .find(|i| i.path.ends_with("withcover.mp4"))
        .expect("embedded");
    assert!(cover.album_art > 0, "attached pic must become ALBUM_ART");
    let art_path = cat.album_art_paths.get(&cover.album_art).expect("art path");
    let bytes = std::fs::read(art_path).unwrap_or_default();
    assert!(is_jpeg_bytes(&bytes), "extracted art must be jpeg");
    let thumb = cat
        .items
        .values()
        .find(|i| i.path.ends_with("bare.mkv"))
        .expect("bare");
    assert!(
        thumb.album_art > 0,
        "video without sidecar/embed gets a thumbnail"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_empty_dirty_attaches_new_poster() {
    let tmp = TempPath::new("art-restat");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    write_fake_mkv(&tmp.join("clip.mkv"), 64);
    let dbp = tmp.join("files.db");
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(dbp.clone()),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    let before = cat
        .items
        .values()
        .find(|i| i.path.ends_with("clip.mkv"))
        .expect("clip");
    let before_art = before.album_art;
    std::fs::write(tmp.join("clip-poster.jpg"), TINY_JPEG).unwrap();
    let (cat2, delta) = monitor(&cfg).unwrap();
    let cat2 = cat2.expect("restat must notice new art");
    let after = cat2
        .items
        .values()
        .find(|i| i.path.ends_with("clip.mkv"))
        .expect("clip after restat");
    assert!(
        after.album_art > 0,
        "periodic/startup monitor attaches poster"
    );
    assert!(
        delta.changed >= 1 || after.album_art != before_art,
        "sidecar must attach or replace a generated thumb"
    );
    let (none, delta2) = monitor(&cfg).unwrap();
    assert!(none.is_none(), "second restat must not rewrite: {delta2:?}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn symlink_alias_preserves_and_adopts_the_physical_files_poster() {
    let tmp = TempPath::new("art-symlink-reconcile");
    let real_dir = tmp.join("real");
    let alias_dir = tmp.join("aliases");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::create_dir_all(&alias_dir).unwrap();
    let media = real_dir.join("movie.mkv");
    let alias = alias_dir.join("movie-link.mkv");
    let poster = real_dir.join("movie-poster.jpg");
    write_fake_mkv(&media, 64);
    std::fs::write(&poster, TINY_JPEG).unwrap();
    std::os::unix::fs::symlink(&media, &alias).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("cache/files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };

    let initial = scan(&cfg).unwrap();
    let physical_art = |catalog: &Catalog| {
        catalog
            .items
            .values()
            .filter(|item| item.ref_id.is_none() && (item.path == media || item.path == alias))
            .map(|item| item.album_art)
            .collect::<Vec<_>>()
    };
    let initial_art = physical_art(&initial);
    assert_eq!(initial_art.len(), 2);
    assert!(initial_art.iter().all(|art_id| *art_id > 0));
    assert!(initial_art.iter().all(|art_id| *art_id == initial_art[0]));
    assert_eq!(initial.album_art_paths[&initial_art[0]], poster);

    let (reconciled, _) = monitor(&cfg).unwrap();
    let reconciled = reconciled.unwrap_or(initial);
    let reconciled_art = physical_art(&reconciled);
    assert!(reconciled_art
        .iter()
        .all(|art_id| *art_id == reconciled_art[0]));
    assert_eq!(reconciled.album_art_paths[&reconciled_art[0]], poster);

    std::fs::remove_file(&poster).unwrap();
    let (removed, _) = monitor(&cfg).unwrap();
    let removed = removed.expect("poster deletion must update aliases");
    assert!(physical_art(&removed).iter().all(|art_id| *art_id == 0));

    std::fs::write(&poster, TINY_JPEG).unwrap();
    let (adopted, _) = monitor(&cfg).unwrap();
    let adopted = adopted.expect("new poster must be adopted");
    let adopted_art = physical_art(&adopted);
    assert!(adopted_art.iter().all(|art_id| *art_id > 0));
    assert!(adopted_art.iter().all(|art_id| *art_id == adopted_art[0]));
    assert_eq!(adopted.album_art_paths[&adopted_art[0]], poster);
}

#[cfg(unix)]
#[test]
fn symlink_only_entry_finds_poster_beside_its_target() {
    let tmp = TempPath::new("art-symlink-only");
    let real_dir = tmp.join("real");
    let alias_dir = tmp.join("aliases");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::create_dir_all(&alias_dir).unwrap();
    let media = real_dir.join("gladiator.mkv");
    let poster = real_dir.join("gladiator-poster.jpg");
    let alias = alias_dir.join("action-movie.mkv");
    write_fake_mkv(&media, 64);
    std::fs::write(&poster, TINY_JPEG).unwrap();
    std::os::unix::fs::symlink(&media, &alias).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        exclude_dirs: vec!["real".into()],
        db_path: Some(tmp.join("cache/files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };

    let catalog = scan(&cfg).unwrap();
    assert!(!catalog
        .items
        .values()
        .any(|item| item.ref_id.is_none() && item.path == media));
    let item = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == alias)
        .expect("symlink-only media entry");
    assert!(item.album_art > 0);
    assert_eq!(catalog.album_art_paths[&item.album_art], poster);
}

#[test]
fn artwork_reconcile_recovers_from_one_item_io_failure_but_not_cancellation() {
    let cfg = ScanConfig::default();
    let timeout = scan_io(
        Path::new("movie.mkv"),
        std::io::Error::new(std::io::ErrorKind::TimedOut, "helper timeout"),
    );
    assert!(recoverable_artwork_error(&cfg, &timeout));
    let interrupted = scan_io(
        Path::new("movie.mkv"),
        std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"),
    );
    assert!(!recoverable_artwork_error(&cfg, &interrupted));
    assert!(recoverable_artwork_error(
        &cfg,
        &ScanError::HelperAdmission(HelperAdmissionError::TimedOut)
    ));

    cfg.cancellation.cancel();
    assert!(!recoverable_artwork_error(&cfg, &timeout));
}

#[test]
fn startup_artwork_timeout_preserves_existing_art_and_continues() {
    let cfg = ScanConfig::default();
    let path = Path::new("movie.mkv");
    let timeout = scan_io(
        path,
        std::io::Error::new(std::io::ErrorKind::TimedOut, "helper timeout"),
    );
    let prepared = recover_prepared_album_art(&cfg, path, Err(timeout)).unwrap();
    assert!(matches!(prepared, PreparedAlbumArt::PreserveExisting));

    let db = LibraryDb::open_memory().unwrap();
    let detail_id = db
        .insert_detail(NewDetail {
            path: "/media/movie.mkv",
            size: 10,
            timestamp: 1,
            title: "movie",
            date: "2024-01-01",
            mime: "video/x-matroska",
            device: 1,
            inode: 1,
            dlna_pn: None,
        })
        .unwrap();
    let art_id = db.upsert_album_art("/cache/old-cover.jpg").unwrap();
    db.set_detail_album_art(detail_id, art_id).unwrap();

    assert!(!apply_index_album_art(&db, detail_id, &prepared).unwrap());
    assert_eq!(db.detail_album_art(detail_id).unwrap(), art_id);
}

#[test]
fn symlink_clones_known_detail_without_second_probe() {
    let tmp = TempPath::new("clone");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("movies")).unwrap();
    std::fs::create_dir_all(tmp.join("genre")).unwrap();
    write_fake_mkv(&tmp.join("movies/film.mkv"), 64);
    std::fs::write(tmp.join("movies/film.nfo"), "<year>1999</year>").unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    let orig = c1.items.values().find(|i| i.title == "film").unwrap();
    assert!(
        orig.date.contains("1999"),
        "nfo date on original {}",
        orig.date
    );
    std::os::unix::fs::symlink(tmp.join("movies/film.mkv"), tmp.join("genre/film-link.mkv"))
        .unwrap();
    let (next, d) = rescan(&cfg, &c1).unwrap();
    assert!(d.added >= 1);
    let alias = next
        .items
        .values()
        .find(|i| i.path.ends_with("film-link.mkv"))
        .expect("symlink alias row");
    assert_eq!(alias.date, orig.date, "cloned DATE, not re-read nfo");
    assert_eq!(alias.mime, orig.mime);
    assert_eq!(alias.device, orig.device);
    assert_eq!(alias.inode, orig.inode);
    assert_ne!(alias.detail_id, orig.detail_id);
    let all_video: Vec<_> = next
        .items
        .values()
        .filter(|i| i.parent_id == VIDEO_ALL_ID)
        .collect();
    assert_eq!(all_video.len(), 1);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn sqlite_files_db_roundtrip_and_delete_original_drops_symlinks() {
    let tmp = TempPath::new("sql");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let orig = tmp.join("video/orig.mkv");
    write_fake_mkv(&orig, 128);
    let link = tmp.join("video/link.mkv");
    std::os::unix::fs::symlink(&orig, &link).unwrap();
    let dbp = tmp.join("files.db");
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(dbp.clone()),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    assert!(cat.items.values().any(|i| i.path.ends_with("orig.mkv")));
    assert!(cat.items.values().any(|i| i.path.ends_with("link.mkv")));
    assert!(dbp.is_file(), "files.db must exist on disk");

    std::fs::remove_file(&orig).unwrap();
    // dangling symlink
    assert!(path_is_symlink(&link));
    assert!(!path_is_live_file(&link));

    let db = LibraryDb::open(&dbp).unwrap();
    let n = db
        .remove_path_and_symlink_aliases(&orig.to_string_lossy())
        .unwrap();
    assert!(n >= 1);
    let cat2 = db.load_catalog().unwrap();
    assert!(!cat2
        .items
        .values()
        .any(|i| i.title == "orig" || i.title == "link"));

    // rescan also prunes
    write_fake_mkv(&orig, 128);
    std::os::unix::fs::symlink(&orig, tmp.join("video/link2.mkv")).ok();
    let _ = scan(&cfg).unwrap();
    std::fs::remove_file(&orig).unwrap();
    let cat3 = scan(&cfg).unwrap();
    assert!(
        !cat3.items.values().any(|i| i.path.ends_with("link2.mkv")),
        "rescan must drop dangling symlink aliases"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn removing_old_path_keeps_live_symlink_aliases() {
    let tmp = TempPath::new("live-alias");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("old")).unwrap();
    std::fs::create_dir_all(tmp.join("action")).unwrap();
    std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
    write_fake_mkv(&tmp.join("old/film.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    std::fs::rename(tmp.join("old/film.mkv"), tmp.join("action/film.mkv")).unwrap();
    std::os::unix::fs::symlink(
        tmp.join("action/film.mkv"),
        tmp.join("genres/action/film.mkv"),
    )
    .unwrap();
    let _ = monitor(&cfg).unwrap();
    let n = forget_path(&cfg, &tmp.join("old/film.mkv")).unwrap();
    let _ = n;
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let cat = db.load_catalog().unwrap();
    assert!(
        cat.items
            .values()
            .any(|i| i.path.ends_with("action/film.mkv")),
        "moved file must stay"
    );
    assert!(
        cat.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("genres/action/film.mkv")),
        "live genre symlink must survive deleting the old path"
    );
    assert!(
        !cat.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("/old/")),
        "old path must be gone"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn rescan_detects_new_and_removed() {
    let tmp = TempPath::new("rescan");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    write_fake_mkv(&tmp.join("video/a.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    write_fake_mkv(&tmp.join("video/b.mkv"), 64);
    let (c2, d) = rescan(&cfg, &c1).unwrap();
    assert!(d.added >= 1);
    assert!(c2.items.values().any(|i| i.title == "b"));
    std::fs::remove_file(tmp.join("video/b.mkv")).unwrap();
    let (c3, d2) = rescan(&cfg, &c2).unwrap();
    assert!(d2.removed >= 1);
    assert!(!c3.items.values().any(|i| i.title == "b"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn video_has_all_recent_and_folders() {
    let tmp = TempPath::new("video-views");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("movies")).unwrap();
    write_fake_mkv(&tmp.join("movies/fresh.mkv"), 64);
    write_fake_mkv(&tmp.join("movies/stale.mkv"), 64);
    let stale = tmp.join("movies/stale.mkv");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(400 * 86_400);
    let _ = std::fs::File::open(&stale).and_then(|f| f.set_modified(old));
    std::os::unix::fs::symlink(
        tmp.join("movies/fresh.mkv"),
        tmp.join("movies/fresh-alias.mkv"),
    )
    .unwrap();
    let cat = scan(&ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    })
    .unwrap();
    let video = cat.children_of(VIDEO_ID).expect("video");
    let titles: Vec<_> = video
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Container(x) => Some(x.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        titles,
        [
            "Actor",
            "All Video",
            "Folders",
            "Genre",
            "Playlists",
            "Rating",
            "Recently Added",
            "Series"
        ]
    );
    assert!(cat.containers.contains_key(VIDEO_DIR_ID));
    let folders = cat.children_of(VIDEO_DIR_ID).expect("folders");
    assert!(
        !folders.is_empty(),
        "Video/Folders must list the media tree"
    );
    let recent = cat.children_of(VIDEO_RECENT_ID).expect("recent");
    let recent_paths: Vec<_> = recent
        .iter()
        .filter_map(|c| match c {
            CatalogChild::Item(i) => Some(i.path.display().to_string()),
            _ => None,
        })
        .collect();
    assert!(
        recent_paths.iter().any(|p| p.ends_with("fresh.mkv")),
        "recent={recent_paths:?}"
    );
    assert!(
        recent_paths.iter().any(|p| p.ends_with("stale.mkv")),
        "no time window: old files stay in recent: {recent_paths:?}"
    );
    assert_eq!(
        recent_paths
            .iter()
            .filter(|p| p.ends_with("fresh.mkv") || p.contains("fresh"))
            .count(),
        1,
        "symlink alias must not duplicate recent: {recent_paths:?}"
    );
    assert!(
        !recent_paths.iter().any(|p| p.contains("alias")),
        "recent must not list symlink alias: {recent_paths:?}"
    );
    let restarted = load_existing(&ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        recent_limit: 1,
        recent_days: Some(90),
        ..Default::default()
    });
    let restarted_recent = restarted.recent_videos();
    assert_eq!(restarted_recent.len(), 1);
    let CatalogChild::Item(restarted_item) = &restarted_recent[0] else {
        panic!("recent must contain an item");
    };
    assert!(restarted_item.path.ends_with("fresh.mkv"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn recent_is_200_unique_inodes_no_time_window() {
    let mut cat = Catalog::new();
    for i in 0..250i64 {
        let oid = format!("64$1${i:X}");
        cat.items.insert(
            oid.clone(),
            MediaItem {
                object_id: oid,
                parent_id: "64$1".into(),
                detail_id: i + 1,
                title: format!("m{i:03}"),
                class: "item.videoItem".into(),
                date: "2024-01-01".into(),
                path: PathBuf::from(format!("/m/{i}.mkv")),
                collection_path: None,
                mime: "video/x-matroska".into(),
                ext: "mkv".into(),
                size: 1000,
                mtime: 1_700_000_000 + i,
                captions: vec![],
                probe: SourceProbe::default(),
                dlna_pn: None,
                ref_id: None,
                device: 8,
                inode: 10_000 + i as u64,
                duration: None,
                bitrate: None,
                resolution: None,
                channels: None,
                samplerate: None,
                album_art: 0,
                creator: None,
                about: None,
                plot: None,
                artist: None,
                album_artist: None,
                composer: None,
                contributor: None,
                album: None,
                genre: None,
                disc: None,
                track: None,
                rating: None,
                rotation: None,
                bookmark_sec: 0,
                watch_count: 0,
            },
        );
        let alias = format!("64$2${i:X}");
        cat.items.insert(
            alias.clone(),
            MediaItem {
                object_id: alias,
                parent_id: "64$2".into(),
                detail_id: 1_000 + i,
                title: format!("m{i:03}-alias"),
                class: "item.videoItem".into(),
                date: "2024-01-01".into(),
                path: PathBuf::from(format!("/genre/{i}.mkv")),
                collection_path: None,
                mime: "video/x-matroska".into(),
                ext: "mkv".into(),
                size: 1000,
                mtime: 1_700_000_000 + i,
                captions: vec![],
                probe: SourceProbe::default(),
                dlna_pn: None,
                ref_id: None,
                device: 8,
                inode: 10_000 + i as u64,
                duration: None,
                bitrate: None,
                resolution: None,
                channels: None,
                samplerate: None,
                album_art: 0,
                creator: None,
                about: None,
                plot: None,
                artist: None,
                album_artist: None,
                composer: None,
                contributor: None,
                album: None,
                genre: None,
                disc: None,
                track: None,
                rating: None,
                rotation: None,
                bookmark_sec: 0,
                watch_count: 0,
            },
        );
    }
    let recent = cat.recent_videos();
    assert_eq!(recent.len(), 200, "cap is 200 unique movies");
    let mut inodes = std::collections::HashSet::new();
    for ch in &recent {
        let CatalogChild::Item(it) = ch else {
            panic!("recent must be items");
        };
        assert!(
            inodes.insert((it.device, it.inode)),
            "duplicate inode in recent {}",
            it.title
        );
        assert!(
            !it.title.contains("alias"),
            "kept original not symlink alias: {}",
            it.title
        );
    }
    // newest 200 unique: mtime 1_700_000_000+50 .. +249
    let CatalogChild::Item(first) = &recent[0] else {
        panic!();
    };
    assert_eq!(first.title, "m249");
    let CatalogChild::Item(last) = &recent[199] else {
        panic!();
    };
    assert_eq!(last.title, "m050");

    let now = 1_700_200_000i64;
    let cutoff = now - 86_400;
    cat.items.get_mut("64$1$F9").unwrap().mtime = now + 60; // future clock skew
    cat.items.get_mut("64$2$F9").unwrap().mtime = now + 60;
    cat.items.get_mut("64$1$F8").unwrap().mtime = cutoff * 1_000_000_000; // exact, nanos
    cat.items.get_mut("64$2$F8").unwrap().mtime = cutoff * 1_000_000_000;
    cat.items.get_mut("64$1$F7").unwrap().mtime = cutoff - 1;
    cat.items.get_mut("64$2$F7").unwrap().mtime = cutoff - 1;
    cat.configure_recent_policy_at(10, Some(1), now);
    let bounded = cat.recent_videos();
    assert_eq!(
        bounded.len(),
        2,
        "window boundary + alias dedup: {bounded:#?}"
    );
    let titles: Vec<_> = bounded
        .iter()
        .filter_map(|child| match child {
            CatalogChild::Item(item) => Some(item.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["m249", "m248"]);
}

#[test]
fn monitor_skips_incomplete_and_only_applies_delta() {
    let tmp = TempPath::new("monitor");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("movies")).unwrap();
    std::fs::create_dir_all(tmp.join("incomplete")).unwrap();
    write_fake_mkv(&tmp.join("movies/keep.mkv"), 64);
    write_fake_mkv(&tmp.join("incomplete/wip.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        exclude_dirs: vec!["incomplete".into()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    assert!(c1.items.values().any(|i| i.title == "keep"));
    assert!(
        !c1.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("incomplete")),
        "incomplete must never be indexed"
    );
    let (none, d0) = monitor(&cfg).unwrap();
    assert!(none.is_none(), "unchanged library must not rewrite");
    assert_eq!(d0, ScanDelta::default());
    write_fake_mkv(&tmp.join("movies/new.mkv"), 64);
    write_fake_mkv(&tmp.join("incomplete/another.mkv"), 64);
    let (some, d) = monitor(&cfg).unwrap();
    let c2 = some.expect("new file");
    assert!(d.added >= 1);
    assert!(c2.items.values().any(|i| i.title == "new"));
    assert!(
        !c2.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("incomplete")),
        "monitor must not pick up incomplete"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn targeted_monitor_does_not_walk_unchanged_roots() {
    let tmp = TempPath::new("targeted-monitor");
    std::fs::create_dir_all(tmp.join("movies")).unwrap();
    let mut paths = Vec::new();
    for index in 0..12 {
        let path = tmp.join(format!("movies/movie-{index:02}.mkv"));
        let mut data = vec![0u8; 32];
        data[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
        std::fs::write(&path, data).unwrap();
        paths.push(path);
    }
    let progress = std::sync::Arc::new(ScanProgress::default());
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        progress: Some(progress.clone()),
        ..Default::default()
    };
    let mut published = scan(&cfg).unwrap();
    let (unchanged, unchanged_delta) = monitor_incremental(&cfg).unwrap();
    assert!(unchanged.is_none());
    assert_eq!(unchanged_delta, ScanDelta::default());

    let dirty = paths[7].clone();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&dirty)
        .unwrap();
    std::io::Write::write_all(&mut file, &[1]).unwrap();
    drop(file);
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&dirty)).unwrap();
    let Some(CatalogUpdate::Patch(patch)) = update else {
        panic!("targeted monitor must produce an incremental patch");
    };
    published.apply_patch(patch);
    assert_eq!(delta.changed, 1);
    let (files_seen, current) = progress.snapshot();
    assert_eq!(files_seen, 1, "only the dirty file should be discovered");
    assert_eq!(current.as_deref(), Some(dirty.as_path()));
    let reloaded = load_existing(&cfg);
    assert_eq!(published.items.len(), reloaded.items.len());
    assert_eq!(published.containers.len(), reloaded.containers.len());
    assert_eq!(
        published.items.keys().collect::<HashSet<_>>(),
        reloaded.items.keys().collect::<HashSet<_>>()
    );
    assert_eq!(
        published.containers.keys().collect::<HashSet<_>>(),
        reloaded.containers.keys().collect::<HashSet<_>>()
    );
}

#[test]
fn incremental_catalog_patch_matches_reload_for_add_sidecar_and_remove() {
    let tmp = TempPath::new("catalog-patch-lifecycle");
    let media = tmp.join("movies/original.mkv");
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    let mut bytes = vec![0u8; 32];
    bytes[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
    std::fs::write(&media, &bytes).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let mut published = scan(&cfg).unwrap();
    let apply = |published: &mut Catalog, update: Option<CatalogUpdate>| {
        let Some(CatalogUpdate::Patch(patch)) = update else {
            panic!("changed targeted monitor must return a patch");
        };
        published.apply_patch(patch);
    };
    let assert_matches_reload = |published: &Catalog| {
        let reloaded = load_existing(&cfg);
        assert_eq!(
            published.items.keys().collect::<HashSet<_>>(),
            reloaded.items.keys().collect::<HashSet<_>>()
        );
        assert_eq!(
            published.containers.keys().collect::<HashSet<_>>(),
            reloaded.containers.keys().collect::<HashSet<_>>()
        );
        for (id, item) in &published.items {
            assert_eq!(
                format!("{item:?}"),
                format!("{:?}", reloaded.items[id]),
                "incremental item {id} differs from a full reload"
            );
        }
        for (id, container) in &published.containers {
            assert_eq!(
                format!("{container:?}"),
                format!("{:?}", reloaded.containers[id]),
                "incremental container {id} differs from a full reload"
            );
        }
        assert_eq!(published.album_art_paths, reloaded.album_art_paths);
        assert_eq!(published.recent_ids, reloaded.recent_ids);
    };

    let added = tmp.join("movies/added.mkv");
    std::fs::write(&added, &bytes).unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&added)).unwrap();
    assert_eq!(delta.added, 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    assert!(published.items.values().any(|item| item.title == "added"));

    let nfo = added.with_extension("nfo");
    std::fs::write(&nfo, "<movie><title>Curated title</title></movie>").unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&nfo)).unwrap();
    assert_eq!(delta.changed, 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    assert!(published
        .items
        .values()
        .any(|item| item.title == "Curated title"));

    let caption = added.with_extension("srt");
    std::fs::write(&caption, "1\n00:00:00,000 --> 00:00:01,000\nCaption\n").unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&caption)).unwrap();
    assert_eq!(delta.changed, 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    assert!(published
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == added)
        .unwrap()
        .captions
        .iter()
        .any(|stored| stored.path == caption));

    let poster = added.with_file_name("added-poster.jpg");
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&poster)).unwrap();
    assert_eq!(delta.changed, 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    let album_art = published
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == added)
        .unwrap()
        .album_art;
    assert_eq!(published.album_art_paths[&album_art], poster);

    let playlist = added.with_file_name("Queue.m3u");
    std::fs::write(&playlist, "added.mkv\n").unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&playlist)).unwrap();
    assert!(delta.changed >= 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    assert!(published.children_of(VIDEO_PLIST_ID).unwrap().iter().any(
        |child| matches!(child, CatalogChild::Container(container) if container.title == "Queue")
    ));

    std::fs::remove_file(&media).unwrap();
    let (update, delta) = monitor_dirty_incremental(&cfg, std::slice::from_ref(&media)).unwrap();
    assert_eq!(delta.removed, 1);
    apply(&mut published, update);
    assert_matches_reload(&published);
    assert!(!published
        .items
        .values()
        .any(|item| item.title == "original"));
}

#[test]
fn load_existing_reads_db_without_walking() {
    let tmp = TempPath::new("load");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    write_fake_mkv(&tmp.join("video/kept.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let scanned = scan(&cfg).unwrap();
    assert!(scanned.items.values().any(|i| i.title == "kept"));
    let loaded = load_existing(&cfg);
    assert!(
        loaded.items.values().any(|i| i.title == "kept"),
        "startup must serve the last files.db without a tree walk"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn fallible_post_publication_reload_does_not_return_an_empty_catalog_on_io_error() {
    let tmp = TempPath::new("published-reload-error");
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg = ScanConfig {
        db_path: Some(tmp.clone()),
        ..Default::default()
    };
    assert!(reload_published_catalog(&cfg).is_err());
}

#[test]
fn parse_media_dir_v_prefix() {
    let (t, p) = parse_media_dir("V,/storage/video");
    assert_eq!(t, MediaTypes::video_only());
    assert_eq!(p, PathBuf::from("/storage/video"));
}

#[test]
fn path_is_under_roots_rejects_escape_symlink() {
    let tmp = TempPath::new("jail");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("video");
    std::fs::create_dir_all(&root).unwrap();
    let inside = root.join("poster.jpg");
    std::fs::write(&inside, b"ok").unwrap();
    assert!(path_is_under_roots(&inside, std::slice::from_ref(&root)));
    let outside = tmp.join("secret.txt");
    std::fs::write(&outside, b"no").unwrap();
    assert!(!path_is_under_roots(&outside, std::slice::from_ref(&root)));
    let link = root.join("escape.jpg");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(path_is_symlink(&link));
    assert!(
        !path_is_under_roots(&link, std::slice::from_ref(&root)),
        "symlink out of media root must fail"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn rooted_open_descriptor_survives_retarget_and_never_opens_escape() {
    use std::io::Read;

    let tmp = TempPath::new("rooted-open-race");
    let root = tmp.join("media");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let inside = root.join("inside.bin");
    let secret = outside.join("secret.bin");
    std::fs::write(&inside, b"allowed-bytes").unwrap();
    std::fs::write(&secret, b"outside-secret").unwrap();
    let alias = root.join("alias.bin");
    std::os::unix::fs::symlink(&inside, &alias).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![root.clone()],
        ..Default::default()
    };

    let mut opened = open_allowed_file(&alias, &cfg).expect("open inside alias");
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    let mut bytes = Vec::new();
    opened.file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"allowed-bytes");
    assert_eq!(
        open_allowed_file(&alias, &cfg).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );

    for index in 0..200 {
        let next = root.join(format!("alias-next-{index}"));
        let target = if index % 2 == 0 { &inside } else { &secret };
        std::os::unix::fs::symlink(target, &next).unwrap();
        std::fs::rename(&next, &alias).unwrap();
        if let Ok(mut opened) = open_allowed_file(&alias, &cfg) {
            let mut bytes = Vec::new();
            opened.file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"allowed-bytes");
        }
    }
}

#[cfg(unix)]
#[test]
fn scanner_nfo_retarget_never_persists_outside_metadata() {
    let tmp = TempPath::new("nfo-descriptor-race");
    let root = tmp.join("media");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let movie = root.join("movie.mkv");
    write_fake_mkv(&movie, 64);
    let allowed = root.join("allowed.nfo");
    let secret = outside.join("secret.nfo");
    std::fs::write(&allowed, "<movie><title>Allowed title</title></movie>").unwrap();
    std::fs::write(
        &secret,
        "<movie><title>Outside secret title</title></movie>",
    )
    .unwrap();
    let selected = root.join("movie.nfo");
    std::os::unix::fs::symlink(&allowed, &selected).unwrap();

    for index in 0..200 {
        let replacement = root.join(format!("nfo-next-{index}"));
        let target = if index % 2 == 0 { &allowed } else { &secret };
        std::os::unix::fs::symlink(target, &replacement).unwrap();
        std::fs::rename(replacement, &selected).unwrap();
        let metadata =
            nfo_for_file_with_policy_result(&movie, std::slice::from_ref(&root), false).unwrap();
        assert_ne!(metadata.title.as_deref(), Some("Outside secret title"));
        if let Some(title) = metadata.title.as_deref() {
            assert_eq!(title, "Allowed title");
        }
    }
}

#[cfg(unix)]
#[test]
fn scanner_jails_directory_links_but_keeps_safe_aliases_and_wide_links_opt_in() {
    let tmp = TempPath::new("tree-jail");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("media");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    write_fake_mkv(&root.join("real/inside.mkv"), 64);
    write_fake_mkv(&outside.join("secret.mkv"), 64);
    std::os::unix::fs::symlink(root.join("real"), root.join("inside-alias")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("outside-alias")).unwrap();
    std::os::unix::fs::symlink(tmp.join("missing"), root.join("broken-alias")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("real/loop-to-root")).unwrap();

    let strict = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let strict_cat = scan(&strict).unwrap();
    assert!(strict_cat
        .items
        .values()
        .any(|item| item.path.ends_with("inside-alias/inside.mkv")));
    assert!(!strict_cat
        .items
        .values()
        .any(|item| item.path.to_string_lossy().contains("outside-alias")));
    assert!(!path_is_allowed_dir(&root.join("outside-alias"), &strict));
    assert!(!path_is_allowed_dir(&root.join("broken-alias"), &strict));

    let wide = ScanConfig {
        media_roots: Vec::new(),
        wide_links: true,
        ..strict.clone()
    };
    let wide_cat = scan(&wide).unwrap();
    assert!(wide_cat
        .items
        .values()
        .any(|item| item.path.ends_with("outside-alias/secret.mkv")));
    assert!(path_is_allowed_file(
        &root.join("outside-alias/secret.mkv"),
        &wide
    ));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn scanner_root_policy_also_jails_nfo_captions_and_artwork() {
    let tmp = TempPath::new("sidecar-jail");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("media");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    write_fake_mkv(&root.join("clip.mkv"), 64);
    std::fs::write(
        outside.join("metadata.nfo"),
        "<movie><title>Escaped</title></movie>",
    )
    .unwrap();
    std::fs::write(outside.join("captions.srt"), "outside subtitle").unwrap();
    std::fs::write(outside.join("poster.jpg"), TINY_JPEG).unwrap();
    std::os::unix::fs::symlink(outside.join("metadata.nfo"), root.join("clip.nfo")).unwrap();
    std::os::unix::fs::symlink(outside.join("captions.srt"), root.join("clip.srt")).unwrap();
    std::os::unix::fs::symlink(outside.join("poster.jpg"), root.join("clip-poster.jpg")).unwrap();

    let strict = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let strict_cat = scan(&strict).unwrap();
    let item = strict_cat
        .items
        .values()
        .find(|item| item.path.ends_with("clip.mkv") && item.ref_id.is_none())
        .unwrap();
    assert_eq!(item.title, "clip");
    assert!(item.captions.is_empty());
    assert_eq!(item.album_art, 0);

    let wide = ScanConfig {
        media_roots: Vec::new(),
        wide_links: true,
        ..strict
    };
    let wide_cat = scan(&wide).unwrap();
    let item = wide_cat
        .items
        .values()
        .find(|item| item.path.ends_with("clip.mkv") && item.ref_id.is_none())
        .unwrap();
    assert_eq!(item.title, "Escaped");
    assert_eq!(item.captions.len(), 1);
    assert!(item.album_art > 0);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn monitor_removes_an_inside_alias_retargeted_outside_the_root() {
    let tmp = TempPath::new("retarget-jail");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("media");
    let inside = root.join("inside");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    write_fake_mkv(&inside.join("movie.mkv"), 64);
    write_fake_mkv(&outside.join("secret.mkv"), 64);
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&inside, &alias).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let first = scan(&cfg).unwrap();
    assert!(first
        .items
        .values()
        .any(|item| item.path.ends_with("alias/movie.mkv")));

    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    let (updated, delta) = monitor(&cfg).unwrap();
    assert!(delta.removed > 0);
    let updated = updated.expect("retarget must publish a catalog update");
    assert!(!updated
        .items
        .values()
        .any(|item| item.path.to_string_lossy().contains("/alias/")));
    assert!(!updated.items.values().any(|item| item.title == "secret"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn rebase_media_path_uses_explicit_root_alias() {
    let tmp = TempPath::new("rebase");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("storage/video");
    let rel = PathBuf::from("shows/clip.mkv");
    write_fake_mkv(&root.join(&rel), 64);
    let stored = tmp.join("storage/pool/video").join(&rel);
    assert!(!stored.exists(), "stored realpath must be absent");
    let cfg = ScanConfig {
        media_roots: vec![MediaRoot {
            configured_path: root.clone(),
            canonical_path: root.canonicalize().unwrap(),
            key: "root-video".into(),
            display_title: "video".into(),
            types: MediaTypes::video_only(),
            aliases: vec![tmp.join("storage/pool/video")],
        }],
        media_dirs: vec![root.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let got = rebase_media_path_for_config(&stored, &cfg);
    assert_eq!(got, root.join(&rel));
    assert!(path_is_live_file(&got));
    let live = root.join(&rel);
    assert_eq!(rebase_media_path_for_config(&live, &cfg), live);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_media_dirs_unions_v_then_a() {
    let (dirs, t) = collect_media_dirs(["V,/storage/video", "A,/storage/audio"]);
    assert_eq!(dirs.len(), 2);
    assert!(t.video, "later A, must not wipe V: {t:?}");
    assert!(t.audio, "A, must be kept: {t:?}");
    assert!(!t.image);
    assert_eq!(dirs[0], PathBuf::from("/storage/video"));
    assert_eq!(dirs[1], PathBuf::from("/storage/audio"));
}

#[test]
fn root_qualified_key_equates_only_explicit_relocation_aliases() {
    let root = PathBuf::from("/storage/video");
    let cfg = ScanConfig {
        media_roots: vec![MediaRoot {
            configured_path: root.clone(),
            canonical_path: root.clone(),
            key: "root-video".into(),
            display_title: "video".into(),
            types: MediaTypes::video_only(),
            aliases: vec![PathBuf::from("/mnt/pool/video")],
        }],
        media_dirs: vec![root.clone()],
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    assert_eq!(
        media_rel_key_for_config(Path::new("/storage/video/Show/ep.mkv"), &cfg),
        "root-video:Show/ep.mkv"
    );
    assert_eq!(
        media_rel_key_for_config(Path::new("/mnt/pool/video/Show/ep.mkv"), &cfg),
        "root-video:Show/ep.mkv"
    );
    assert!(paths_are_same_media(
        "/mnt/pool/video/Show/ep.mkv",
        Path::new("/storage/video/Show/ep.mkv"),
        &cfg
    ));
    assert!(path_is_under_watched(
        "/mnt/pool/video/Show/S01/ep.mkv",
        Path::new("/storage/video/Show"),
        &cfg
    ));
}

#[test]
fn root_qualified_path_normalization_is_reversible_and_collision_free() {
    let roots = [
        ("video-root", "/srv/video", "/mnt/video"),
        ("audio-root", "/srv/audio", "/mnt/audio"),
    ];
    let cfg = ScanConfig {
        media_roots: roots
            .iter()
            .map(|(key, configured, alias)| MediaRoot {
                configured_path: PathBuf::from(configured),
                canonical_path: PathBuf::from(configured),
                key: (*key).into(),
                display_title: (*key).into(),
                types: MediaTypes::all(),
                aliases: vec![PathBuf::from(alias)],
            })
            .collect(),
        media_dirs: roots
            .iter()
            .map(|(_, configured, _)| PathBuf::from(configured))
            .collect(),
        types: MediaTypes::all(),
        ..Default::default()
    };

    let relatives = ["one.mkv", "nested/two.flac", "space name/三.jpg"];
    let mut keys = HashSet::new();
    for (root_key, configured, alias) in roots {
        for relative in relatives {
            let canonical_key =
                media_rel_key_for_config(&PathBuf::from(configured).join(relative), &cfg);
            let alias_key = media_rel_key_for_config(&PathBuf::from(alias).join(relative), &cfg);
            assert_eq!(canonical_key, alias_key);
            assert!(canonical_key.starts_with(&format!("{root_key}:")));
            assert!(keys.insert(canonical_key), "normalized path collision");
        }
    }
}

#[test]
fn media_root_validation_rejects_missing_duplicate_nested_and_same_basename() {
    let tmp = TempPath::new("root-validation");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("one/child")).unwrap();
    std::fs::create_dir_all(tmp.join("two/child")).unwrap();

    assert!(build_media_roots(["missing"], &tmp)
        .unwrap_err()
        .contains("does not exist"));
    assert!(build_media_roots(["one", "one"], &tmp)
        .unwrap_err()
        .contains("duplicate"));
    assert!(build_media_roots(["one", "one/child"], &tmp)
        .unwrap_err()
        .contains("nested"));
    assert!(build_media_roots(["one/child", "two/child"], &tmp)
        .unwrap_err()
        .contains("distinct directory names"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn per_root_masks_keys_and_persisted_relocation_survive_reconcile() {
    let tmp = TempPath::new("root-identity");
    let old_parent = tmp.join("host");
    let new_parent = tmp.join("container");
    let video = old_parent.join("video");
    let audio = old_parent.join("audio");
    std::fs::create_dir_all(video.join("Show")).unwrap();
    std::fs::create_dir_all(audio.join("Show")).unwrap();
    write_fake_mkv(&video.join("Show/episode.mkv"), 64);
    write_fake_mkv(&audio.join("wrong-video.mkv"), 64);
    let mut flac = b"fLaC".to_vec();
    flac.extend_from_slice(&[0; 48]);
    std::fs::write(audio.join("Show/episode.flac"), &flac).unwrap();
    std::fs::write(video.join("wrong-audio.flac"), &flac).unwrap();
    let db_path = tmp.join("cache/files.db");

    let mut roots = build_media_roots(
        [
            format!("V,{}", video.display()),
            format!("A,{}", audio.display()),
        ],
        &tmp,
    )
    .unwrap();
    load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap();
    let cfg = ScanConfig {
        media_dirs: roots
            .iter()
            .map(|root| root.configured_path.clone())
            .collect(),
        media_roots: roots,
        db_path: Some(db_path.clone()),
        types: MediaTypes::all(),
        ..Default::default()
    };
    assert_ne!(
        media_rel_key_for_config(&video.join("Show/same.name"), &cfg),
        media_rel_key_for_config(&audio.join("Show/same.name"), &cfg),
        "identical relative paths in different roots must remain distinct"
    );
    let first = scan(&cfg).unwrap();
    let titles: HashSet<_> = first
        .items
        .values()
        .map(|item| item.title.as_str())
        .collect();
    assert!(titles.contains("episode"));
    assert!(!titles.contains("wrong-video"));
    assert!(!titles.contains("wrong-audio"));
    let episode_paths: HashSet<_> = first
        .items
        .values()
        .filter(|item| item.title == "episode")
        .map(|item| item.path.clone())
        .collect();
    assert_eq!(episode_paths.len(), 2);

    std::fs::create_dir_all(&new_parent).unwrap();
    std::fs::rename(&video, new_parent.join("video")).unwrap();
    std::fs::rename(&audio, new_parent.join("audio")).unwrap();
    let mut moved_roots = build_media_roots(
        [
            format!("V,{}", new_parent.join("video").display()),
            format!("A,{}", new_parent.join("audio").display()),
        ],
        &tmp,
    )
    .unwrap();
    load_and_persist_media_root_mappings(&mut moved_roots, &db_path).unwrap();
    assert!(
        moved_roots.iter().all(|root| !root.aliases.is_empty()),
        "old host paths must be explicit aliases"
    );
    let moved = ScanConfig {
        media_dirs: moved_roots
            .iter()
            .map(|root| root.configured_path.clone())
            .collect(),
        media_roots: moved_roots,
        db_path: Some(db_path),
        types: MediaTypes::all(),
        ..Default::default()
    };
    let (published, delta) = monitor(&moved).unwrap();
    assert!(
        published.is_none(),
        "relocation alone must not rewrite catalog rows"
    );
    assert_eq!(delta, ScanDelta::default());
    for item in load_existing(&moved).items.values() {
        let live = rebase_media_path_for_config(&item.path, &moved);
        assert!(live.is_file(), "{} did not rebase", item.path.display());
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collection_sources_backfill_unchanged_files_and_follow_same_inode_alias_retargets() {
    use std::os::unix::fs::symlink;
    let tmp = TempPath::new("collection-reconciliation");
    let root = tmp.join("media");
    let first = root.join("Briar Saga/01 - The Garden (2001).mkv");
    let second = root.join("Briar Saga/02 - The Lantern (2003).mkv");
    let alias = root.join("genres/Adventure/01 - The Garden (2001).mkv");
    std::fs::create_dir_all(first.parent().unwrap()).unwrap();
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    write_fake_mkv(&first, 64);
    std::fs::hard_link(&first, &second).unwrap();
    symlink(&first, &alias).unwrap();
    let database = tmp.join("cache/files.db");
    let cfg = ScanConfig {
        media_dirs: vec![root],
        db_path: Some(database.clone()),
        ..Default::default()
    };
    let mut catalog = scan(&cfg).unwrap();
    let with_collection_bytes = catalog.estimated_memory_bytes();
    let collection_bytes: u64 = catalog
        .items
        .values_mut()
        .filter_map(|item| item.collection_path.take())
        .map(|path| path.as_os_str().as_encoded_bytes().len() as u64)
        .sum();
    assert!(collection_bytes > 0);
    assert_eq!(
        with_collection_bytes - catalog.estimated_memory_bytes(),
        collection_bytes
    );
    let sequence = || {
        let catalog = load_existing(&cfg);
        let item = catalog
            .items
            .values()
            .find(|item| item.path == alias)
            .unwrap();
        video_collection(item.collection_path.as_deref().unwrap(), &item.mime)
            .unwrap()
            .sequence
    };
    assert_eq!(sequence(), 1);
    {
        let db = rusqlite::Connection::open(&database).unwrap();
        db.execute_batch("UPDATE DETAILS SET COLLECTION_PATH = NULL; PRAGMA user_version=12;")
            .unwrap();
    }
    let (published, delta) = monitor(&cfg).unwrap();
    assert!(published.is_some());
    assert!(
        delta.changed > 0,
        "collection backfill must publish even without file changes"
    );
    assert_eq!(sequence(), 1);
    std::fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();
    let (published, delta) = monitor(&cfg).unwrap();
    assert!(published.is_some());
    assert!(
        delta.changed > 0,
        "equal inode, size and mtime must not hide an alias retarget"
    );
    assert_eq!(sequence(), 2);
    assert_eq!(monitor(&cfg).unwrap().1, ScanDelta::default());
}

#[test]
fn ambiguous_historical_root_aliases_leave_roots_and_settings_unchanged() {
    let tmp = TempPath::new("root-mapping-rollback");
    let video = tmp.join("current/video");
    let audio = tmp.join("current/audio");
    std::fs::create_dir_all(&video).unwrap();
    std::fs::create_dir_all(&audio).unwrap();
    let mut roots = build_media_roots(
        [
            format!("V,{}", video.display()),
            format!("A,{}", audio.display()),
        ],
        &tmp,
    )
    .unwrap();
    let original_roots = roots.clone();
    let db_path = tmp.join("database/files.db");
    let db = open_library_db(&db_path).unwrap();
    let historical = tmp.join("historical/library");
    let seeded = [
        (
            format!("media_root:{}:configured", roots[0].key),
            path_to_db(&historical),
        ),
        (
            format!("media_root:{}:canonical", roots[0].key),
            path_to_db(&tmp.join("historical-video-canonical")),
        ),
        (
            format!("media_root:{}:configured", roots[1].key),
            path_to_db(&historical.join("nested")),
        ),
        (
            format!("media_root:{}:canonical", roots[1].key),
            path_to_db(&tmp.join("historical-audio-canonical")),
        ),
    ];
    for (key, value) in &seeded {
        db.set_setting(key, value).unwrap();
    }
    let settings_before = seeded
        .iter()
        .map(|(key, _)| (key.clone(), db.setting(key).unwrap()))
        .collect::<Vec<_>>();
    drop(db);

    let error = load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap_err();
    assert!(error.contains("ambiguous"), "{error}");
    assert_eq!(roots, original_roots, "caller roots must not be mutated");

    let reopened = open_library_db(&db_path).unwrap();
    let settings_after = seeded
        .iter()
        .map(|(key, _)| (key.clone(), reopened.setting(key).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(settings_after, settings_before);
}

#[test]
fn media_root_mapping_write_failure_rolls_back_prior_settings_and_roots() {
    let tmp = TempPath::new("root-mapping-write-rollback");
    let media = tmp.join("current/video");
    std::fs::create_dir_all(&media).unwrap();
    let mut roots = build_media_roots([format!("V,{}", media.display())], &tmp).unwrap();
    let original_roots = roots.clone();
    let configured_key = format!("media_root:{}:configured", roots[0].key);
    let canonical_key = format!("media_root:{}:canonical", roots[0].key);
    let seeded = [
        (
            configured_key.clone(),
            path_to_db(&tmp.join("old-configured")),
        ),
        (
            canonical_key.clone(),
            path_to_db(&tmp.join("old-canonical")),
        ),
    ];
    assert_ne!(seeded[0].1, path_to_db(&roots[0].configured_path));
    assert_ne!(seeded[1].1, path_to_db(&roots[0].canonical_path));
    let db_path = tmp.join("database/files.db");
    let db = open_library_db(&db_path).unwrap();
    for (key, value) in &seeded {
        db.set_setting(key, value).unwrap();
    }
    db.connection()
        .execute_batch(&format!(
            "CREATE TRIGGER fail_media_root_canonical_update
             BEFORE UPDATE ON SETTINGS
             WHEN NEW.KEY = '{canonical_key}'
             BEGIN
               SELECT RAISE(ABORT, 'injected media-root setting failure');
             END;"
        ))
        .unwrap();
    drop(db);

    let error = load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap_err();
    assert!(
        error.contains("injected media-root setting failure"),
        "{error}"
    );
    assert_eq!(roots, original_roots);
    let reopened = open_library_db(&db_path).unwrap();
    for (key, value) in &seeded {
        assert_eq!(
            reopened.setting(key).unwrap().as_deref(),
            Some(value.as_str())
        );
    }
}

fn media_root_settings(db: &LibraryDb) -> Vec<(String, String)> {
    let mut statement = db
        .connection()
        .prepare(
            "SELECT KEY, VALUE FROM SETTINGS
             WHERE substr(KEY, 1, length('media_root:')) = 'media_root:'
             ORDER BY KEY COLLATE BINARY",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn historical_root_aliases_survive_multiple_fresh_restarts_and_non_utf8_moves() {
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempPath::new("root-history-chain");
    let root_a = tmp.join("a/video");
    std::fs::create_dir_all(&root_a).unwrap();
    let media_a = root_a.join("movie.mkv");
    write_fake_mkv(&media_a, 64);
    let db_path = tmp.join("cache/files.db");
    let mut roots_a = build_media_roots([format!("V,{}", root_a.display())], &tmp).unwrap();
    load_and_persist_media_root_mappings(&mut roots_a, &db_path).unwrap();
    let key = roots_a[0].key.clone();
    let cfg_a = ScanConfig {
        media_dirs: vec![root_a.clone()],
        media_roots: roots_a,
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg_a).unwrap();
    let detail_id = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media_a)
        .unwrap()
        .detail_id;
    LibraryDb::open(&db_path)
        .unwrap()
        .update_bookmark(detail_id, Some(222), Some(3))
        .unwrap();

    let non_utf_parent = tmp.join(OsString::from_vec(b"b-\x80".to_vec()));
    let root_b = non_utf_parent.join("video");
    std::fs::create_dir_all(&non_utf_parent).unwrap();
    std::fs::rename(&root_a, &root_b).unwrap();
    let make_root = |path: &Path| MediaRoot {
        configured_path: path.to_path_buf(),
        canonical_path: std::fs::canonicalize(path).unwrap(),
        key: key.clone(),
        display_title: "video".into(),
        types: MediaTypes::video_only(),
        aliases: Vec::new(),
    };
    let mut roots_b = vec![make_root(&root_b)];
    load_and_persist_media_root_mappings(&mut roots_b, &db_path).unwrap();
    assert!(roots_b[0].aliases.contains(&root_a));

    let mut fresh_b = vec![make_root(&root_b)];
    load_and_persist_media_root_mappings(&mut fresh_b, &db_path).unwrap();
    assert!(fresh_b[0].aliases.contains(&root_a));
    let cfg_b = ScanConfig {
        media_dirs: vec![root_b.clone()],
        media_roots: fresh_b,
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    assert_eq!(monitor(&cfg_b).unwrap().1, ScanDelta::default());

    let root_c = tmp.join("c/video");
    std::fs::create_dir_all(root_c.parent().unwrap()).unwrap();
    std::fs::rename(&root_b, &root_c).unwrap();
    let mut roots_c = vec![make_root(&root_c)];
    load_and_persist_media_root_mappings(&mut roots_c, &db_path).unwrap();
    assert!(roots_c[0].aliases.contains(&root_a));
    assert!(roots_c[0].aliases.contains(&root_b));
    let cfg_c = ScanConfig {
        media_dirs: vec![root_c.clone()],
        media_roots: roots_c,
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    assert_eq!(monitor(&cfg_c).unwrap().1, ScanDelta::default());
    let db = LibraryDb::open(&db_path).unwrap();
    let row = db
        .find_detail_by_path(&path_to_db(&media_a))
        .unwrap()
        .unwrap();
    assert_eq!(row.id, detail_id);
    assert_eq!(db.get_bookmark(detail_id).unwrap(), Some((222, 3)));
}

#[test]
fn removed_root_cleanup_and_delete_failure_are_transactional() {
    let tmp = TempPath::new("root-history-delete");
    let keep = tmp.join("keep");
    std::fs::create_dir_all(&keep).unwrap();
    let db_path = tmp.join("files.db");
    let mut roots = build_media_roots([format!("V,{}", keep.display())], &tmp).unwrap();
    load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap();
    let db = LibraryDb::open(&db_path).unwrap();
    db.set_setting("media_root:removed:configured", "/old/removed")
        .unwrap();
    let before = media_root_settings(&db);
    db.connection()
        .execute_batch(
            "CREATE TRIGGER fail_removed_root_delete
             BEFORE DELETE ON SETTINGS
             WHEN OLD.KEY = 'media_root:removed:configured'
             BEGIN
               SELECT RAISE(ABORT, 'injected removed-root delete failure');
             END;",
        )
        .unwrap();
    drop(db);
    let original_roots = roots.clone();
    let error = load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap_err();
    assert!(
        error.contains("injected removed-root delete failure"),
        "{error}"
    );
    assert_eq!(roots, original_roots);
    let db = LibraryDb::open(&db_path).unwrap();
    assert_eq!(media_root_settings(&db), before);
    db.connection()
        .execute_batch("DROP TRIGGER fail_removed_root_delete")
        .unwrap();
    drop(db);

    let mut no_roots = Vec::new();
    load_and_persist_media_root_mappings(&mut no_roots, &db_path).unwrap();
    assert!(media_root_settings(&LibraryDb::open(&db_path).unwrap()).is_empty());
}

#[test]
fn hostile_persisted_root_map_is_rejected_before_materialization_or_mutation() {
    let tmp = TempPath::new("root-history-bound");
    let media = tmp.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let db_path = tmp.join("files.db");
    let db = LibraryDb::open(&db_path).unwrap();
    for index in 0..=4096 {
        db.set_setting(&format!("media_root:hostile:{index:05}"), "x")
            .unwrap();
    }
    let before = media_root_settings(&db);
    drop(db);
    let mut roots = build_media_roots([format!("V,{}", media.display())], &tmp).unwrap();
    let original = roots.clone();
    let error = load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap_err();
    assert!(error.contains("bounded settings budget"), "{error}");
    assert_eq!(roots, original);
    assert_eq!(
        media_root_settings(&LibraryDb::open(&db_path).unwrap()),
        before
    );
}

#[test]
fn oversized_desired_root_map_rolls_back_before_any_setting_or_caller_change() {
    let tmp = TempPath::new("root-history-desired-bound");
    let db_path = tmp.join("files.db");
    let mut roots = Vec::new();
    for root_index in 0..63 {
        let configured = tmp.join(format!("configured-{root_index}"));
        let aliases = (0..64)
            .map(|alias_index| tmp.join(format!("alias-{root_index}-{alias_index}")))
            .collect();
        roots.push(MediaRoot {
            configured_path: configured.clone(),
            canonical_path: configured,
            key: format!("root-{root_index}"),
            display_title: format!("root-{root_index}"),
            types: MediaTypes::video_only(),
            aliases,
        });
    }
    let original = roots.clone();
    let error = load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap_err();
    assert!(error.contains("desired media-root mappings"), "{error}");
    assert_eq!(roots, original);
    assert!(media_root_settings(&LibraryDb::open(&db_path).unwrap()).is_empty());
}

#[test]
fn every_supported_extension_scans_with_canonical_mime_class_and_image_probe() {
    let tmp = TempPath::new("format-map");
    std::fs::create_dir_all(&tmp).unwrap();
    let video_fixture = tmp.join("fixture-video.mkv");
    write_fake_mkv(&video_fixture, 64);
    let audio_fixture = tmp.join("fixture-audio.wav");
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
        let path = tmp.join(format!("format-{index}.{}", format.extension));
        match resolved.kind {
            MediaKind::Video => std::fs::copy(&video_fixture, &path).unwrap(),
            MediaKind::Audio => std::fs::copy(&audio_fixture, &path).unwrap(),
            MediaKind::Image => {
                std::fs::write(&path, TINY_JPEG).unwrap();
                TINY_JPEG.len() as u64
            }
        };
        expected.push((path, resolved));

        if format.is_ambiguous() {
            let audio_path = tmp.join(format!("audio-only-{index}.{}", format.extension));
            std::fs::copy(&audio_fixture, &audio_path).unwrap();
            expected.push((audio_path, format.resolve(Some(MediaKind::Audio))));
        }
    }
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::all(),
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    for (path, resolved) in expected {
        let items: Vec<_> = catalog
            .items
            .values()
            .filter(|item| item.path == path)
            .collect();
        assert!(!items.is_empty(), "{} was not indexed", path.display());
        for item in items {
            assert_eq!(item.mime, resolved.mime, "{}", path.display());
            assert_eq!(item.class, resolved.upnp_class(), "{}", path.display());
            assert_ne!(item.mime, "application/octet-stream");
            if resolved.kind == MediaKind::Image {
                assert_eq!(item.probe.container, "jpeg");
                assert!(item.probe.video.is_empty());
                assert!(item.probe.audio.is_empty());
                assert!(item.probe.hdr.is_empty());
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

fn container_named<'a>(cat: &'a Catalog, parent: &str, title: &str) -> Option<&'a str> {
    let c = cat.containers.get(parent)?;
    c.children.iter().find_map(|ch| {
        let cc = cat.containers.get(ch)?;
        (cc.title == title).then_some(ch.as_str())
    })
}

fn item_titles(cat: &Catalog, parent: &str) -> Vec<String> {
    let Some(c) = cat.containers.get(parent) else {
        return Vec::new();
    };
    let mut t: Vec<String> = c
        .children
        .iter()
        .filter_map(|ch| cat.items.get(ch).map(|i| i.title.clone()))
        .collect();
    t.sort();
    t
}

#[test]
fn dir_symlink_does_not_duplicate_real_folder() {
    let tmp = TempPath::new("dirlink");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("video");
    std::fs::create_dir_all(root.join("kids/Movies/Despicable Me")).unwrap();
    write_fake_mkv(
        &root.join("kids/Movies/Despicable Me/01 - Despicable Me.mkv"),
        64,
    );
    write_fake_mkv(
        &root.join("kids/Movies/Despicable Me/02 - Despicable Me 2.mkv"),
        64,
    );
    std::fs::write(
        root.join("kids/Movies/Despicable Me/01 - Despicable Me.nfo"),
        "<movie><genre>Animation</genre></movie>",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("genres/BY_YEAR/2010/Movies")).unwrap();
    std::os::unix::fs::symlink(
        root.join("kids/Movies/Despicable Me"),
        root.join("genres/BY_YEAR/2010/Movies/Despicable Me"),
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    let kids = container_named(&cat, BROWSEDIR_ID, "video")
        .and_then(|id| container_named(&cat, id, "kids"))
        .and_then(|id| container_named(&cat, id, "Movies"))
        .and_then(|id| container_named(&cat, id, "Despicable Me"))
        .expect("kids/Movies/Despicable Me");
    let year = container_named(&cat, BROWSEDIR_ID, "video")
        .and_then(|id| container_named(&cat, id, "genres"))
        .and_then(|id| container_named(&cat, id, "BY_YEAR"))
        .and_then(|id| container_named(&cat, id, "2010"))
        .and_then(|id| container_named(&cat, id, "Movies"))
        .and_then(|id| container_named(&cat, id, "Despicable Me"))
        .expect("BY_YEAR/2010/Movies/Despicable Me");
    assert_ne!(kids, year, "symlink dir must keep its own folder id");
    assert_eq!(
        item_titles(&cat, kids),
        vec![
            "01 - Despicable Me".to_string(),
            "02 - Despicable Me 2".to_string()
        ]
    );
    assert_eq!(
        item_titles(&cat, year),
        vec![
            "01 - Despicable Me".to_string(),
            "02 - Despicable Me 2".to_string()
        ]
    );
    let animation =
        container_named(&cat, VIDEO_GENRE_ID, "Animation").expect("Animation genre container");
    assert_eq!(
        item_titles(&cat, animation),
        vec!["01 - Despicable Me"],
        "physical video must occur once in a virtual view"
    );

    let rebuilt = rebuild_objects(&cfg).unwrap();
    let kids = container_named(&rebuilt, BROWSEDIR_ID, "video")
        .and_then(|id| container_named(&rebuilt, id, "kids"))
        .and_then(|id| container_named(&rebuilt, id, "Movies"))
        .and_then(|id| container_named(&rebuilt, id, "Despicable Me"))
        .expect("rebuilt kids folder");
    assert_eq!(
        item_titles(&rebuilt, kids).len(),
        2,
        "rebuild must not dump year-alias clones into kids/: {:?}",
        item_titles(&rebuilt, kids)
    );
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    assert!(
        !db.folders_have_duplicate_inodes().unwrap(),
        "no inode+title twice in one folder"
    );
    let alias_detail = db
        .find_detail_by_path(&path_to_db(
            &root.join("genres/BY_YEAR/2010/Movies/Despicable Me/01 - Despicable Me.mkv"),
        ))
        .unwrap()
        .unwrap()
        .id;
    db.upsert_object(
        &format!("{animation}$DEADBEEF"),
        animation,
        "item.videoItem",
        Some(alias_detail),
        "01 - Despicable Me",
        None,
    )
    .unwrap();
    assert!(db.folders_have_duplicate_inodes().unwrap());
    drop(db);

    let (repaired, delta) = repair_objects_if_needed(&cfg).unwrap();
    assert_eq!(delta.removed, 1);
    assert_eq!(
        item_titles(&repaired.unwrap(), animation),
        vec!["01 - Despicable Me"]
    );
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    assert!(!db.folders_have_duplicate_inodes().unwrap());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_drops_moved_file_and_empty_folder() {
    let tmp = TempPath::new("move");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("old")).unwrap();
    std::fs::create_dir_all(tmp.join("new")).unwrap();
    write_fake_mkv(&tmp.join("old/episode.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    assert!(c1
        .items
        .values()
        .any(|i| i.path.ends_with("old/episode.mkv")));
    std::fs::rename(tmp.join("old/episode.mkv"), tmp.join("new/episode.mkv")).unwrap();
    let (c2, d) = rescan(&cfg, &c1).unwrap();
    assert!(d.removed >= 1, "move must drop the source: {d:?}");
    assert!(d.added >= 1, "move must index the dest: {d:?}");
    assert!(
        !c2.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("/old/")),
        "source path must leave the catalog"
    );
    assert!(c2
        .items
        .values()
        .any(|i| i.path.ends_with("new/episode.mkv")));
    assert!(
        !c2.containers.values().any(|c| c.title == "old"),
        "empty source folder must be pruned"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn forget_path_matches_host_realpath_prefix() {
    let tmp = TempPath::new("forget");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("mnt/video");
    std::fs::create_dir_all(root.join("Show")).unwrap();
    write_fake_mkv(&root.join("Show/ep.mkv"), 64);
    let old_root = tmp.join("host/z2/video");
    let cfg = ScanConfig {
        media_roots: vec![MediaRoot {
            configured_path: root.clone(),
            canonical_path: root.canonicalize().unwrap(),
            key: "root-video".into(),
            display_title: "video".into(),
            types: MediaTypes::video_only(),
            aliases: vec![old_root.clone()],
        }],
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    let dbp = cfg.db_path.as_ref().unwrap();
    {
        let conn = rusqlite::Connection::open(dbp).unwrap();
        conn.execute(
            "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
            rusqlite::params![
                old_root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                root.join("Show/ep.mkv").to_string_lossy().as_ref(),
            ],
        )
        .unwrap();
    }
    let n = forget_path(&cfg, &root.join("Show/ep.mkv")).unwrap();
    assert!(n >= 1, "inotify mount path must delete the realpath row");
    let db = LibraryDb::open(dbp).unwrap();
    let left = db
        .all_detail_stats()
        .unwrap()
        .into_iter()
        .filter(|row| row.path.ends_with("ep.mkv"))
        .count();
    assert_eq!(left, 0);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_does_not_rewrite_equivalent_realpath_prefix() {
    let tmp = TempPath::new("equiv");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("mnt/video");
    std::fs::create_dir_all(root.join("Show")).unwrap();
    write_fake_mkv(&root.join("Show/ep.mkv"), 64);
    let old_root = tmp.join("host/z2/video");
    let cfg = ScanConfig {
        media_roots: vec![MediaRoot {
            configured_path: root.clone(),
            canonical_path: root.canonicalize().unwrap(),
            key: "root-video".into(),
            display_title: "video".into(),
            types: MediaTypes::video_only(),
            aliases: vec![old_root.clone()],
        }],
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    {
        let conn = rusqlite::Connection::open(cfg.db_path.as_ref().unwrap()).unwrap();
        conn.execute(
            "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
            rusqlite::params![
                old_root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                root.join("Show/ep.mkv").to_string_lossy().as_ref(),
            ],
        )
        .unwrap();
    }
    let (none, d) = monitor(&cfg).unwrap();
    assert!(
        none.is_none(),
        "same file under host realpath must not be rewritten: {d:?}"
    );
    assert_eq!(d, ScanDelta::default());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn next_child_seq_skips_gaps_so_upsert_cannot_rename_a_folder() {
    let tmp = TempPath::new("seq");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let dbp = tmp.join("files.db");
    let db = LibraryDb::open(&dbp).unwrap();
    db.seed_virtual_containers().unwrap();
    db.upsert_object("64$1", "64", "container.storageFolder", None, "video", None)
        .unwrap();
    db.upsert_object(
        "64$1$1",
        "64$1",
        "container.storageFolder",
        None,
        "keep",
        None,
    )
    .unwrap();
    db.upsert_object(
        "64$1$2",
        "64$1",
        "container.storageFolder",
        None,
        "mid",
        None,
    )
    .unwrap();
    db.upsert_object(
        "64$1$3",
        "64$1",
        "container.storageFolder",
        None,
        "sport",
        None,
    )
    .unwrap();
    drop(db);
    // Delete $1 only. count(*)+1 == 3, which is still "sport".
    {
        let conn = rusqlite::Connection::open(&dbp).unwrap();
        conn.execute("DELETE FROM OBJECTS WHERE OBJECT_ID = '64$1$1'", [])
            .unwrap();
    }
    let db = LibraryDb::open(&dbp).unwrap();
    let next = db.next_child_seq("64$1").unwrap();
    assert_eq!(next, 4, "must be max(2,3)+1, not count(*)+1=3");
    assert!(db.object_exists("64$1$3").unwrap());
    let name = db.object_name("64$1$3").unwrap().unwrap();
    assert_eq!(name, "sport");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn sqlite_integer_boundaries_fail_closed_without_wrapping_ids_or_sizes() {
    assert_eq!(sqlite_file_size(i64::MAX as u64).unwrap(), i64::MAX);
    assert!(matches!(
        sqlite_file_size(i64::MAX as u64 + 1),
        Err(ScanError::Invariant(message)) if message.contains("file size")
    ));
    for value in [0, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        assert_eq!(
            u64_from_sqlite_i64_bits(sqlite_i64_from_u64_bits(value)),
            value
        );
    }

    let tmp = TempPath::new("seq-overflow");
    std::fs::create_dir_all(&tmp).unwrap();
    let db = LibraryDb::open(&tmp.join("files.db")).unwrap();
    db.seed_virtual_containers().unwrap();
    db.upsert_object(
        "64$7FFFFFFFFFFFFFFF",
        "64",
        "container.storageFolder",
        None,
        "last child",
        None,
    )
    .unwrap();
    assert!(matches!(
        db.next_child_seq("64"),
        Err(rusqlite::Error::IntegralValueOutOfRange(_, i64::MAX))
    ));
}

#[test]
fn catalog_load_rejects_a_negative_persisted_media_size() {
    let tmp = TempPath::new("negative-catalog-size");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("movie.mkv"), 64);
    let db_path = tmp.join("files.db");
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let db = LibraryDb::open(&db_path).unwrap();
    db.connection()
        .execute("UPDATE DETAILS SET SIZE = -1 WHERE MIME IS NOT NULL", [])
        .unwrap();
    assert!(matches!(
        db.load_catalog(),
        Err(rusqlite::Error::IntegralValueOutOfRange(7, -1))
    ));
}

#[test]
fn full_and_incremental_catalog_load_reject_an_exhausted_detail_id() {
    let tmp = TempPath::new("exhausted-detail-id");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    write_fake_mkv(&media_dir.join("movie.mkv"), 64);
    let db_path = tmp.join("files.db");
    let cfg = ScanConfig {
        media_dirs: vec![media_dir],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let original_id = catalog.items.values().next().unwrap().detail_id;

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "UPDATE DETAILS SET ID=?1 WHERE ID=?2",
        rusqlite::params![i64::MAX, original_id],
    )
    .unwrap();
    raw.execute(
        "UPDATE OBJECTS SET DETAIL_ID=?1 WHERE DETAIL_ID=?2",
        rusqlite::params![i64::MAX, original_id],
    )
    .unwrap();
    drop(raw);

    let db = LibraryDb::open(&db_path).unwrap();
    assert!(matches!(
        db.load_catalog(),
        Err(rusqlite::Error::IntegralValueOutOfRange(4, i64::MAX))
    ));
    db.begin_catalog_change_capture().unwrap();
    db.connection()
        .execute("UPDATE DETAILS SET TITLE='changed' WHERE ID=?1", [i64::MAX])
        .unwrap();
    assert!(matches!(
        db.load_catalog_patch(),
        Err(rusqlite::Error::IntegralValueOutOfRange(4, i64::MAX))
    ));
}

#[test]
fn new_folder_after_sibling_delete_does_not_inherit_old_children() {
    let tmp = TempPath::new("id-reuse");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("keep")).unwrap();
    std::fs::create_dir_all(tmp.join("mid")).unwrap();
    std::fs::create_dir_all(tmp.join("sport")).unwrap();
    write_fake_mkv(&tmp.join("keep/keep.mkv"), 64);
    write_fake_mkv(&tmp.join("mid/mid.mkv"), 64);
    write_fake_mkv(&tmp.join("sport/game.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    let sport = c1
        .containers
        .values()
        .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
        .expect("sport folder")
        .clone();
    assert!(c1
        .items
        .values()
        .any(|i| i.parent_id == sport.object_id && i.title == "game"));

    std::fs::remove_file(tmp.join("keep/keep.mkv")).unwrap();
    let _ = std::fs::remove_dir(tmp.join("keep"));
    let (c2, _) = rescan(&cfg, &c1).unwrap();
    let sport2 = c2
        .containers
        .values()
        .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
        .expect("sport survives")
        .clone();
    assert_eq!(sport2.object_id, sport.object_id);

    std::fs::create_dir_all(tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H")).unwrap();
    write_fake_mkv(
        &tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H/ep.mkv"),
        64,
    );
    let (c3, d) = rescan(&cfg, &c2).unwrap();
    assert!(d.added >= 1, "Fallout episode must be added: {d:?}");

    let fallout = c3
        .containers
        .values()
        .find(|c| c.title.starts_with("Fallout.S01") && c.object_id.starts_with("64"))
        .expect("Fallout folder");
    assert_ne!(
        fallout.object_id, sport2.object_id,
        "Fallout must get a new id, not sport's"
    );
    let fallout_titles: Vec<_> = c3
        .items
        .values()
        .filter(|i| i.parent_id == fallout.object_id)
        .map(|i| i.title.as_str())
        .collect();
    assert!(
        fallout_titles.contains(&"ep"),
        "Fallout children={fallout_titles:?}"
    );
    assert!(
        !fallout_titles.contains(&"game"),
        "sport file leaked into Fallout: {fallout_titles:?}"
    );
    let sport3 = c3
        .containers
        .values()
        .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
        .expect("sport must keep its name");
    assert!(
        c3.items
            .values()
            .any(|i| i.parent_id == sport3.object_id && i.title == "game"),
        "sport/game.mkv must stay under sport"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_second_pass_does_not_readd_existing_files() {
    let tmp = TempPath::new("noop");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("action")).unwrap();
    std::fs::create_dir_all(tmp.join("genres/crime")).unwrap();
    write_fake_mkv(&tmp.join("action/film.mkv"), 64);
    std::fs::write(
        tmp.join("action/film.nfo"),
        "<movie><genre>Crime</genre></movie>",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        tmp.join("action/film.mkv"),
        tmp.join("genres/crime/film.mkv"),
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    db.connection()
        .execute("UPDATE DETAILS SET STREAM_PROBE_REV = 0, GENRE = NULL", [])
        .unwrap();
    drop(db);
    let (first, d1) = monitor(&cfg).unwrap();
    let _ = first;
    let _ = d1;
    let (second, d2) = monitor(&cfg).unwrap();
    assert!(
        second.is_none(),
        "unchanged library must not rewrite: {d2:?}"
    );
    assert_eq!(d2.added, 0, "must not count already-indexed files as adds");
    assert_eq!(d2.removed, 0);
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let reprobed: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM DETAILS WHERE STREAM_PROBE_REV != 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reprobed, 0, "periodic NFO refresh reopened unchanged media");
    let restored_genres: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM DETAILS WHERE GENRE = 'Crime'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        restored_genres, 2,
        "NFO override was not restored to aliases"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn symlink_dir_alias_does_not_steal_original_object() {
    let tmp = TempPath::new("steal");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("action/Now You See Me")).unwrap();
    std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
    write_fake_mkv(&tmp.join("action/Now You See Me/film.mkv"), 64);
    std::os::unix::fs::symlink(
        tmp.join("action/Now You See Me"),
        tmp.join("genres/action/Now You See Me"),
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    let orig_path = tmp.join("action/Now You See Me/film.mkv");
    let orig = c1
        .items
        .values()
        .find(|i| i.path == orig_path && i.ref_id.is_none())
        .expect("original")
        .clone();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let alias = tmp.join("genres/action/Now You See Me/film.mkv");
    let folder = ensure_folder_chain(&db, &cfg, &alias)
        .unwrap()
        .expect("genre folder chain");
    assert!(
        folder.contains('$'),
        "genre path must get its own folder id: {folder}"
    );
    assert_ne!(
        folder, orig.parent_id,
        "symlink-dir walk must not collapse onto the original folder"
    );
    assert!(index_one_file(&db, &cfg, &alias, &folder).unwrap());
    let c2 = db.load_catalog().unwrap();
    let still = c2.items.get(&orig.object_id).expect("original object kept");
    assert_eq!(
        still.detail_id, orig.detail_id,
        "alias must not steal the original OBJECTS.DETAIL_ID"
    );
    assert_eq!(still.parent_id, orig.parent_id);
    assert!(
        c2.items.values().any(|i| {
            i.path.ends_with("genres/action/Now You See Me/film.mkv") && i.parent_id == folder
        }),
        "alias should live under the genre folder"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn disc_structure_is_not_indexed() {
    let tmp = TempPath::new("bdmv");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("movie/BDMV/STREAM")).unwrap();
    write_fake_mkv(&tmp.join("movie/title.mkv"), 64);
    write_fake_mkv(&tmp.join("movie/BDMV/STREAM/00001.m2ts"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    assert!(cat.items.values().any(|i| i.title == "title"));
    assert!(
        !cat.items
            .values()
            .any(|i| i.path.to_string_lossy().contains("BDMV")),
        "BDMV streams must not be catalogued"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_readds_real_path_kept_only_as_dir_symlink_alias() {
    let tmp = TempPath::new("realias");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("kids/Movies/The Incredibles")).unwrap();
    std::fs::create_dir_all(tmp.join("genres/BY_YEAR/2004/Movies")).unwrap();
    let real = tmp.join("kids/Movies/The Incredibles/02 - Incredibles 2.mkv");
    write_fake_mkv(&real, 64);
    std::os::unix::fs::symlink(
        tmp.join("kids/Movies/The Incredibles"),
        tmp.join("genres/BY_YEAR/2004/Movies/The Incredibles"),
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    let n = forget_path(&cfg, &real).unwrap();
    assert!(n >= 1, "real path row must drop; live alias stays: {n}");
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let after_forget = db.all_detail_stats().unwrap();
    assert!(
        after_forget
            .iter()
            .any(|row| row.path.contains("genres/BY_YEAR")),
        "dir-symlink alias must survive deleting the real path: {after_forget:?}"
    );
    assert!(
        !after_forget.iter().any(|row| row
            .path
            .ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
        "real path must be gone before monitor: {after_forget:?}"
    );
    drop(db);
    let (some, d) = monitor(&cfg).unwrap();
    let _ = some;
    assert!(
        d.added >= 1,
        "monitor must reindex the live real path: {d:?}"
    );
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let rows = db.all_detail_stats().unwrap();
    assert!(
        rows.iter().any(|row| row
            .path
            .ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
        "real path back in DETAILS: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row.path.contains("genres/BY_YEAR") && row.path.ends_with("02 - Incredibles 2.mkv")
        }),
        "alias path must stay: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn monitor_rename_updates_real_path_and_dir_symlink_alias() {
    let tmp = TempPath::new("renalias");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("action/Jason Bourne")).unwrap();
    std::fs::create_dir_all(tmp.join("genres/BY_YEAR/2004/Movies")).unwrap();
    let old = tmp.join("action/Jason Bourne/old.mkv");
    write_fake_mkv(&old, 64);
    std::os::unix::fs::symlink(
        tmp.join("action/Jason Bourne"),
        tmp.join("genres/BY_YEAR/2004/Movies/Jason Bourne"),
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    assert!(c1
        .items
        .values()
        .any(|i| i.path.ends_with("action/Jason Bourne/old.mkv")));
    assert!(c1.items.values().any(
        |i| i.path.to_string_lossy().contains("genres/BY_YEAR") && i.path.ends_with("old.mkv")
    ));
    let new = tmp.join("action/Jason Bourne/02 - The Bourne Supremacy.mkv");
    std::fs::rename(&old, &new).unwrap();
    let (c2, d) = rescan(&cfg, &c1).unwrap();
    assert!(d.removed >= 1, "old name must leave: {d:?}");
    assert!(d.added >= 1, "new name must be indexed: {d:?}");
    assert!(
        c2.items.values().any(|i| i
            .path
            .ends_with("action/Jason Bourne/02 - The Bourne Supremacy.mkv")),
        "real folder must list the new name"
    );
    assert!(
        c2.items.values().any(|i| {
            let p = i.path.to_string_lossy();
            p.contains("genres/BY_YEAR") && p.ends_with("02 - The Bourne Supremacy.mkv")
        }),
        "dir-symlink alias must list the new name"
    );
    assert!(
        !c2.items.values().any(|i| i.path.ends_with("old.mkv")),
        "old name must be gone from every alias"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn rebuild_objects_drops_missing_details() {
    let tmp = TempPath::new("rebuild-miss");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let keep = tmp.join("video/keep.mkv");
    let gone = tmp.join("video/gone.mkv");
    write_fake_mkv(&keep, 64);
    write_fake_mkv(&gone, 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    std::fs::remove_file(&gone).unwrap();
    let _ = rebuild_objects(&cfg).unwrap();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let rows = db.all_detail_stats().unwrap();
    assert!(
        rows.iter().any(|row| row.path.ends_with("keep.mkv")),
        "live file stays"
    );
    assert!(
        !rows.iter().any(|row| row.path.ends_with("gone.mkv")),
        "missing file must leave DETAILS, not just OBJECTS: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn rebuild_objects_keeps_browse_folder_ids() {
    let tmp = TempPath::new("rebuild-ids");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    write_fake_mkv(&tmp.join("video/keep.mkv"), 64);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let first = scan(&cfg).unwrap();
    let before: Vec<String> = first
        .items
        .values()
        .filter(|i| i.path.ends_with("keep.mkv"))
        .map(|i| i.object_id.clone())
        .collect();
    assert!(!before.is_empty(), "scan must index keep.mkv");
    let _ = rebuild_objects(&cfg).unwrap();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let after = db.load_catalog().unwrap();
    for id in &before {
        assert!(
            after.items.contains_key(id) || after.containers.contains_key(id),
            "rebuild must keep ObjectID {id} (Infuse caches it)"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn probe_from_stored_uses_resolution_and_keeps_empty() {
    let p = probe_from_stored(
        "mkv",
        Some("mkv"),
        Some("hevc"),
        Some("truehd,ac3"),
        Some("1:0:truehd:8,2:1:ac3:6"),
        Some("dv-p8"),
        Some("3840x2160"),
    );
    assert_eq!(p.width, 3840);
    assert_eq!(p.height, 2160);
    assert_eq!(p.audio, "truehd,ac3");
    assert_eq!(primary_codec(&p.audio), "truehd");
    let empty = probe_from_stored("mkv", None, None, None, None, None, None);
    assert!(empty.video.is_empty() && empty.hdr.is_empty() && empty.audio.is_empty());
    assert_eq!(empty.width, 0);
    assert_eq!(empty.container, "mkv");
}

#[test]
fn probe_from_stored_preserves_bounded_raw_metadata_and_lenient_marker_defaults() {
    let raw = concat!(
        "1:0:aac:2,",
        "@v:Main%2010:not-a-level:bad%XX:not-a-depth:tail%:%FF:5.1:ignored,",
        "@t:unknown:mode"
    );
    let probe = probe_from_stored(
        "mp4",
        Some("mp4"),
        Some("hevc"),
        Some("aac"),
        Some(raw),
        Some("sdr"),
        Some("1920x1080"),
    );
    assert_eq!(probe.audio_streams, raw);
    assert_eq!(probe.video_profile, "Main 10");
    assert_eq!(probe.video_level, 0);
    assert_eq!(probe.pixel_format, "");
    assert_eq!(probe.bit_depth, 0);
    assert_eq!(probe.frame_rate, "tail%");
    assert_eq!(probe.codec_string, "");
    assert_eq!(probe.audio_layout, "5.1");
    assert_eq!(probe.video_timestamp_mode, "unknown:mode");
}

#[test]
fn probe_from_stored_discards_over_budget_compact_metadata() {
    let oversized = "x".repeat(rusty_dlna_protocol::MAX_COMPACT_STREAM_METADATA_BYTES + 1);
    let probe = probe_from_stored(
        "mp4",
        Some("mp4"),
        Some("h264"),
        Some("aac"),
        Some(&oversized),
        Some("sdr"),
        Some("1920x1080"),
    );
    assert!(probe.audio_streams.is_empty());
    assert!(probe.video_profile.is_empty());
    assert_eq!(probe.video_level, 0);
    assert!(probe.pixel_format.is_empty());
    assert_eq!(probe.bit_depth, 0);
    assert!(probe.frame_rate.is_empty());
    assert!(probe.codec_string.is_empty());
    assert!(probe.audio_layout.is_empty());
    assert!(probe.video_timestamp_mode.is_empty());
    assert_eq!(probe.video, "h264");
    assert_eq!(probe.audio, "aac");
}

#[test]
fn probe_from_stored_avi_other_is_mpeg4() {
    let p = probe_from_stored(
        "avi",
        Some("avi"),
        Some("other"),
        Some("ac3"),
        Some("1:0:ac3:6"),
        Some("sdr"),
        Some("720x480"),
    );
    assert_eq!(p.video, "mpeg4");
    assert_eq!(p.width, 720);
    assert_eq!(p.height, 480);
    assert_eq!(
        dlna_pn_from_probe(&p.container, &p.video, &p.audio, &p.hdr, p.width, p.height).as_deref(),
        Some("MPEG4_P2_AVI_ASP_L5_SO")
    );
}

#[test]
fn dlna_pn_mkv_hevc_stays_empty_mp4_is_written() {
    assert_eq!(
        dlna_pn_from_probe("mkv", "hevc", "truehd", "dv-p8", 3840, 2160),
        None
    );
    assert_eq!(
        dlna_pn_from_probe("mp4", "h264", "aac", "sdr", 1920, 1080).as_deref(),
        Some("AVC_MP4_MP_HD_AAC_MULT5")
    );
    assert_eq!(
        dlna_pn_from_probe("mp4", "hevc", "eac3", "dv-p8", 3840, 2160).as_deref(),
        Some("HEVC_MP4_BL_Main10_L5_HD1080_AC3")
    );
}

#[test]
fn apply_probe_writes_dlna_pn_and_multi_audio() {
    let db = LibraryDb::open_memory().unwrap();
    let id = db
        .insert_detail(NewDetail {
            path: "/tmp/clip.mp4",
            size: 10,
            timestamp: 1,
            title: "clip",
            date: "2024-01-01",
            mime: "video/mp4",
            device: 1,
            inode: 1,
            dlna_pn: None,
        })
        .unwrap();
    let got = MediaProbe {
        probe: SourceProbe {
            container: "mp4".into(),
            video: "h264".into(),
            hdr: "sdr".into(),
            audio: "aac,ac3".into(),
            audio_streams: "1:0:aac:2,2:1:ac3:6".into(),
            width: 1920,
            height: 800,
            ..SourceProbe::default()
        },
        av: AvMeta {
            duration: Some("1:00:00.000".into()),
            resolution: Some("1920x800".into()),
            channels: Some(2),
            samplerate: Some(48000),
            ..AvMeta::default()
        },
        tags: EmbeddedTags::default(),
        audio_tracks: Vec::new(),
        chapters: Vec::new(),
    };
    apply_probe_to_detail(&db, id, &got).unwrap();
    db.upsert_object("64$1$1", "64$1", "item.videoItem", Some(id), "clip", None)
        .unwrap();
    let cat = db.load_catalog().unwrap();
    let it = cat
        .items
        .values()
        .find(|i| i.detail_id == id)
        .expect("item");
    assert_eq!(it.probe.audio, "aac,ac3");
    assert_eq!(it.probe.audio_streams, "1:0:aac:2,2:1:ac3:6");
    assert_eq!(it.probe.width, 1920);
    assert_eq!(it.probe.height, 800);
    assert_eq!(it.dlna_pn.as_deref(), Some("AVC_MP4_MP_HD_AAC_MULT5"));
}

#[test]
fn sidecar_applies_when_libav_fails() {
    let tmp = TempPath::new("sidecar");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let p = tmp.join("video/dvp7.mp4");
    write_incomplete_mp4(&p, 64 * 1024);
    std::fs::write(
        tmp.join("video/dvp7.probe.toml"),
        "container = \"mkv\"\nvideo = \"hevc\"\nhdr = \"dv-p7\"\naudio = \"truehd\"\n",
    )
    .unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = scan(&cfg).unwrap();
    let it = cat
        .items
        .values()
        .find(|i| i.path.ends_with("dvp7.mp4"))
        .expect("indexed");
    assert_eq!(it.probe.hdr, "dv-p7", "{it:?}");
    assert_eq!(it.probe.video, "hevc");
    assert_eq!(it.probe.audio, "truehd");
    let _ = std::fs::remove_dir_all(&tmp);
}

pub(super) fn stored_probe_fields(
    db_path: &Path,
    media: &Path,
) -> (String, String, String, String) {
    let db = LibraryDb::open(db_path).unwrap();
    db.connection()
        .query_row(
            "SELECT COALESCE(CONTAINER, ''), COALESCE(VIDEO, ''), COALESCE(HDR, ''),
                    COALESCE(PROBE_SIDECAR_FINGERPRINT, '')
             FROM DETAILS WHERE PATH = ?1",
            [path_to_db(media)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

#[test]
fn named_and_stem_probe_sidecars_converge_on_write_change_delete_and_restart() {
    let tmp = TempPath::new("probe-sidecar-dirty");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let media = media_dir.join("movie.mkv");
    write_fake_mkv(&media, 64);
    let stem_sidecar = media_dir.join("movie.probe.toml");
    let named_sidecar = media_dir.join("movie.mkv.probe.toml");
    std::fs::write(&stem_sidecar, "container=\"mkv\"\nvideo=\"hevc\"\n").unwrap();
    let db_path = tmp.join("files.db");
    let gate = std::sync::Arc::new(HelperGate::new(2, 8));
    let cfg = ScanConfig {
        media_dirs: vec![media_dir.clone()],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        helper_gate: Some(gate.clone()),
        ..Default::default()
    };

    let mut catalog = scan(&cfg).unwrap();
    let stem = stored_probe_fields(&db_path, &media);
    assert_eq!(
        (&stem.0, &stem.1),
        (&"mkv".to_string(), &"hevc".to_string())
    );

    std::fs::write(&named_sidecar, "container=\"mp4\"\nvideo=\"h264\"\n").unwrap();
    let (update, delta) =
        monitor_dirty_incremental(&cfg, std::slice::from_ref(&named_sidecar)).unwrap();
    let Some(CatalogUpdate::Patch(patch)) = update else {
        panic!("probe-sidecar write must produce an incremental patch");
    };
    catalog.apply_patch(patch);
    assert_eq!(delta.changed, 1);
    let named = stored_probe_fields(&db_path, &media);
    assert_eq!(
        (&named.0, &named.1),
        (&"mp4".to_string(), &"h264".to_string())
    );
    assert_ne!(named.3, stem.3);
    let reloaded = load_existing(&cfg);
    let patched = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap();
    let full = reloaded
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == media)
        .unwrap();
    assert_eq!(
        (
            &patched.probe.container,
            &patched.probe.video,
            &patched.probe.audio,
            &patched.probe.hdr,
        ),
        (
            &full.probe.container,
            &full.probe.video,
            &full.probe.audio,
            &full.probe.hdr,
        )
    );

    std::fs::write(
        &named_sidecar,
        "# formatting changes do not matter\ncontainer = \"mp4\"\nvideo = \"h264\"\n",
    )
    .unwrap();
    let (published, delta) = monitor_dirty(&cfg, std::slice::from_ref(&named_sidecar)).unwrap();
    assert!(
        published.is_none(),
        "equal semantics must not publish a patch"
    );
    assert_eq!(delta, ScanDelta::default());

    std::fs::remove_file(&named_sidecar).unwrap();
    monitor_dirty(&cfg, std::slice::from_ref(&named_sidecar)).unwrap();
    assert_eq!(stored_probe_fields(&db_path, &media), stem);

    std::fs::remove_file(&stem_sidecar).unwrap();
    monitor_dirty(&cfg, std::slice::from_ref(&stem_sidecar)).unwrap();
    let raw = stored_probe_fields(&db_path, &media);
    assert_eq!(raw.3, NO_PROBE_SIDECAR_FINGERPRINT);
    assert_ne!(raw.1, "hevc");

    let probes_before = gate.metrics().admitted_total;
    std::fs::write(
        &named_sidecar,
        "# no effective override\nunknown = \"ignored\"\n",
    )
    .unwrap();
    let (published, delta) = monitor_dirty(&cfg, std::slice::from_ref(&named_sidecar)).unwrap();
    assert!(published.is_none());
    assert_eq!(delta, ScanDelta::default());
    assert_eq!(gate.metrics().admitted_total, probes_before);
    std::fs::remove_file(&named_sidecar).unwrap();

    std::fs::write(&stem_sidecar, "container=\"mpeg-ts\"\nvideo=\"mpeg2\"\n").unwrap();
    let (full, _) = monitor(&cfg).unwrap();
    assert!(full.is_some());
    assert_eq!(
        stored_probe_fields(&db_path, &media),
        (
            "mpeg-ts".into(),
            "mpeg2".into(),
            "sdr".into(),
            probe_sidecar_fingerprint(&parse_probe_toml(
                "container=\"mpeg-ts\"\nvideo=\"mpeg2\"\n"
            ))
        )
    );
}

#[test]
fn stem_probe_sidecar_targets_all_same_stem_media_but_named_has_precedence() {
    let tmp = TempPath::new("probe-sidecar-owners");
    std::fs::create_dir_all(&*tmp).unwrap();
    let mkv = tmp.join("movie.mkv");
    let mp4 = tmp.join("movie.mp4");
    write_fake_mkv(&mkv, 64);
    write_fake_mkv(&mp4, 64);
    let stem = tmp.join("movie.probe.toml");
    let named = tmp.join("movie.mkv.probe.toml");
    std::fs::write(&stem, "video=\"hevc\"\n").unwrap();
    std::fs::write(&named, "video=\"h264\"\n").unwrap();
    let db_path = tmp.join("files.db");
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    scan(&cfg).unwrap();
    assert_eq!(stored_probe_fields(&db_path, &mkv).1, "h264");
    assert_eq!(stored_probe_fields(&db_path, &mp4).1, "hevc");

    std::fs::write(&stem, "video=\"mpeg2\"\n").unwrap();
    monitor_dirty(&cfg, std::slice::from_ref(&stem)).unwrap();
    assert_eq!(stored_probe_fields(&db_path, &mkv).1, "h264");
    assert_eq!(stored_probe_fields(&db_path, &mp4).1, "mpeg2");
}

#[cfg(unix)]
#[test]
fn probe_sidecar_candidates_and_owners_preserve_non_utf8_names() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let tmp = TempPath::new("probe-sidecar-non-utf");
    std::fs::create_dir_all(&*tmp).unwrap();
    let media = tmp.join(OsString::from_vec(b"movie-\x80.mkv".to_vec()));
    write_fake_mkv(&media, 64);
    let [named, stem] = probe_sidecar_candidates(&media);
    assert_eq!(
        named.file_name().unwrap().as_bytes(),
        b"movie-\x80.mkv.probe.toml"
    );
    assert_eq!(
        stem.file_name().unwrap().as_bytes(),
        b"movie-\x80.probe.toml"
    );
    std::fs::write(&named, "video=\"hevc\"\n").unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![tmp.clone()],
        types: MediaTypes::video_only(),
        thumbnails: false,
        ..Default::default()
    };
    assert_eq!(probe_sidecar_owner_paths(&cfg, &named).unwrap(), [media]);
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn file_symlink_aliases_keep_distinct_probe_overlays_with_one_raw_probe() {
    let tmp = TempPath::new("probe-sidecar-symlink-alias");
    let real_dir = tmp.join("real");
    let alias_dir = tmp.join("alias");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::create_dir_all(&alias_dir).unwrap();
    let real = real_dir.join("movie.mkv");
    let alias = alias_dir.join("link.mkv");
    write_fake_mkv(&real, 64);
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let real_sidecar = real_dir.join("movie.probe.toml");
    let alias_sidecar = alias_dir.join("link.probe.toml");
    std::fs::write(&real_sidecar, "video=\"hevc\"\nhdr=\"dv-p7\"\n").unwrap();
    std::fs::write(&alias_sidecar, "video=\"h264\"\nhdr=\"sdr\"\n").unwrap();
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
    assert_eq!(gate.metrics().admitted_total, 1);
    let real_before = stored_probe_fields(&db_path, &real);
    let alias_before = stored_probe_fields(&db_path, &alias);
    assert_eq!(real_before.1, "hevc");
    assert_eq!(alias_before.1, "h264");
    assert_eq!(real_before.2, "dv-p7");
    assert_eq!(alias_before.2, "sdr");
    assert_ne!(real_before.3, alias_before.3);

    let before = gate.metrics().admitted_total;
    std::fs::write(&alias_sidecar, "video=\"mpeg2\"\nhdr=\"sdr\"\n").unwrap();
    monitor_dirty(&cfg, std::slice::from_ref(&alias_sidecar)).unwrap();
    assert_eq!(gate.metrics().admitted_total - before, 1);
    assert_eq!(stored_probe_fields(&db_path, &real), real_before);
    assert_eq!(stored_probe_fields(&db_path, &alias).1, "mpeg2");

    let before = gate.metrics().admitted_total;
    std::fs::remove_file(&alias_sidecar).unwrap();
    monitor_dirty(&cfg, std::slice::from_ref(&alias_sidecar)).unwrap();
    assert_eq!(gate.metrics().admitted_total - before, 1);
    assert_eq!(stored_probe_fields(&db_path, &real), real_before);
    assert_eq!(
        stored_probe_fields(&db_path, &alias).3,
        NO_PROBE_SIDECAR_FINGERPRINT
    );
}

#[test]
fn backfill_rewrites_avi_other_and_pn() {
    let db = LibraryDb::open_memory().unwrap();
    let id = db
        .insert_detail(NewDetail {
            path: "/media/clip.avi",
            size: 10,
            timestamp: 1,
            title: "clip",
            date: "2024-01-01",
            mime: "video/x-msvideo",
            device: 1,
            inode: 2,
            dlna_pn: None,
        })
        .unwrap();
    db.update_detail_stream(
        id,
        DetailStreamUpdate {
            duration: Some("0:01:00.000"),
            resolution: Some("720x480"),
            channels: Some(2),
            samplerate: Some(48000),
            container: Some("avi"),
            video: Some("other"),
            audio: Some("ac3"),
            hdr: Some("sdr"),
            ..DetailStreamUpdate::default()
        },
    )
    .unwrap();
    let n = db.backfill_derived_stream_fields().unwrap();
    assert!(n >= 1, "expected derived rewrite, n={n}");
    db.upsert_object("64$1$1", "64$1", "item.videoItem", Some(id), "clip", None)
        .unwrap();
    let cat = db.load_catalog().unwrap();
    let it = cat.items.values().find(|i| i.detail_id == id).unwrap();
    assert_eq!(it.probe.video, "mpeg4");
    assert_eq!(it.probe.width, 720);
    assert_eq!(it.dlna_pn.as_deref(), Some("MPEG4_P2_AVI_ASP_L5_SO"));
}

#[test]
fn stream_probe_fill_keyset_uses_the_inode_index_without_a_temp_sort() {
    let db = LibraryDb::open_memory().unwrap();
    let explain = format!(
        "EXPLAIN QUERY PLAN {}",
        crate::db::STREAM_PROBE_NONNULL_BATCH_SQL
    );
    let mut statement = db.connection().prepare(&explain).unwrap();
    let plan = statement
        .query_map(
            rusqlite::params![
                crate::db::STREAM_PROBE_REVISION,
                i64::MIN,
                i64::MIN,
                i64::MIN,
                512
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("IDX_DETAILS_INODE"), "query plan: {plan}");
    assert!(!plan.contains("TEMP B-TREE"), "query plan: {plan}");
}

#[test]
fn monitor_updates_replaced_file_and_relinked_aliases() {
    let tmp = TempPath::new("inode-replace");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let a = tmp.join("video/orig.mp4");
    let b = tmp.join("video/alias.mp4");
    write_incomplete_mp4(&a, 64 * 1024);
    std::fs::hard_link(&a, &b).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let _ = scan(&cfg).unwrap();
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    let first_a = db
        .find_detail_by_path(&a.to_string_lossy())
        .unwrap()
        .unwrap();
    let first_b = db
        .find_detail_by_path(&b.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(first_a.size, 64 * 1024);
    assert_eq!(first_a.inode, first_b.inode);

    std::fs::remove_file(&a).unwrap();
    write_incomplete_mp4(&a, 256 * 1024);
    let (_, d) = monitor(&cfg).unwrap();
    assert!(d.changed >= 1, "replace must count as a change: {d:?}");
    let second_a = db
        .find_detail_by_path(&a.to_string_lossy())
        .unwrap()
        .unwrap();
    let second_b = db
        .find_detail_by_path(&b.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(second_a.size, 256 * 1024, "replaced path must get new size");
    assert_ne!(
        second_a.inode, first_a.inode,
        "replaced path must get new inode"
    );
    assert_eq!(
        second_b.size,
        64 * 1024,
        "untouched hardlink stays on old file"
    );
    assert_eq!(second_b.inode, first_b.inode);

    std::fs::remove_file(&b).unwrap();
    std::fs::hard_link(&a, &b).unwrap();
    let (_, d2) = monitor(&cfg).unwrap();
    assert!(d2.changed >= 1, "relink must update alias: {d2:?}");
    let third_b = db
        .find_detail_by_path(&b.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(third_b.size, 256 * 1024);
    assert_eq!(third_b.inode, second_a.inode);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn failed_probe_retries_after_size_change() {
    let tmp = TempPath::new("reprobe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("video")).unwrap();
    let p = tmp.join("video/growing.mp4");
    write_incomplete_mp4(&p, 64 * 1024);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let c1 = scan(&cfg).unwrap();
    let it = c1
        .items
        .values()
        .find(|i| i.path.ends_with("growing.mp4"))
        .expect("indexed while incomplete");
    assert!(
        it.probe.hdr.is_empty() && it.duration.is_none(),
        "failed probe must not store fake sdr: {it:?}"
    );
    let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
    assert!(
        db.details_missing_stream_meta().unwrap().is_empty(),
        "a failed attempt must be recorded instead of retried forever"
    );
    let (_, unchanged) = monitor(&cfg).unwrap();
    assert_eq!(
        unchanged,
        ScanDelta::default(),
        "an unchanged failed file must not trigger another probe"
    );
    write_fake_mkv(&p, 0);
    let (c2, d) = rescan(&cfg, &c1).unwrap();
    assert!(
        d.changed >= 1
            || c2
                .items
                .values()
                .any(|i| { i.path.ends_with("growing.mp4") && i.duration.is_some() }),
        "size change must re-probe: {d:?}"
    );
    let it2 = c2
        .items
        .values()
        .find(|i| i.path.ends_with("growing.mp4"))
        .expect("still indexed");
    assert!(
        it2.duration.is_some() && !it2.probe.hdr.is_empty(),
        "finished file must get stream metadata: {it2:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn corrupt_database_is_backed_up_before_fresh_recovery() {
    let tmp = TempPath::new("corrupt-recovery");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("files.db");
    let corrupt = b"this is deliberately not a sqlite database";
    std::fs::write(&path, corrupt).unwrap();

    let db = open_library_db(&path).unwrap();
    assert_eq!(db.detail_count().unwrap(), 0);
    drop(db);
    let backups: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("files.db.corrupt-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "backup files: {backups:?}");
    assert_eq!(std::fs::read(&backups[0]).unwrap(), corrupt);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn failed_scan_transaction_preserves_previous_catalog_generation() {
    let tmp = TempPath::new("atomic-scan");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("video");
    std::fs::create_dir_all(&root).unwrap();
    write_fake_mkv(&root.join("old.mkv"), 4096);
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let before = scan(&cfg).unwrap();
    assert!(before
        .items
        .values()
        .any(|item| item.path.ends_with("old.mkv")));
    let before_ids: HashSet<_> = before.items.keys().cloned().collect();
    {
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        db.connection()
            .execute_batch(
                "CREATE TRIGGER fail_new_detail BEFORE INSERT ON DETAILS BEGIN
                       SELECT RAISE(ABORT, 'injected detail failure');
                     END;",
            )
            .unwrap();
    }
    write_fake_mkv(&root.join("new.mkv"), 4096);
    let error = scan_refresh(&cfg).unwrap_err();
    assert!(error.to_string().contains("injected detail failure"));

    let after = load_existing(&cfg);
    let after_ids: HashSet<_> = after.items.keys().cloned().collect();
    assert_eq!(after_ids, before_ids);
    assert!(!after
        .items
        .values()
        .any(|item| item.path.ends_with("new.mkv")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cancelled_scan_rolls_back_its_open_transaction() {
    let tmp = TempPath::new("cancelled-transaction");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("video");
    std::fs::create_dir_all(&root).unwrap();
    write_fake_mkv(&root.join("old.mkv"), 4096);
    let base = ScanConfig {
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    scan(&base).unwrap();
    write_fake_mkv(&root.join("new.mkv"), 4096);

    let gate = std::sync::Arc::new(HelperGate::new(1, 1));
    let held = gate.try_acquire().unwrap();
    let cancellation = CancellationToken::default();
    let cancelled_cfg = ScanConfig {
        helper_gate: Some(std::sync::Arc::clone(&gate)),
        helper_queue_timeout: std::time::Duration::from_secs(30),
        cancellation: cancellation.clone(),
        ..base.clone()
    };
    let scan_thread = std::thread::spawn(move || scan_refresh(&cancelled_cfg));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while gate.metrics().queued != 1 {
        assert!(std::time::Instant::now() < deadline, "probe did not queue");
        std::thread::yield_now();
    }
    cancellation.cancel();
    assert!(matches!(
        scan_thread.join().unwrap(),
        Err(ScanError::Cancelled)
    ));
    drop(held);

    let after = load_existing(&base);
    assert!(after
        .items
        .values()
        .any(|item| item.path.ends_with("old.mkv")));
    assert!(!after
        .items
        .values()
        .any(|item| item.path.ends_with("new.mkv")));
}

#[test]
fn tagged_audio_populates_metadata_views_and_sidecar_precedence() {
    let tmp = TempPath::new("audio-tags");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("music");
    std::fs::create_dir_all(&root).unwrap();
    let audio = root.join("tagged.flac");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.1",
            "-c:a",
            "flac",
            "-metadata",
            "title=Tagged Song",
            "-metadata",
            "artist=Track Artist",
            "-metadata",
            "album_artist=Album Artist",
            "-metadata",
            "album=Tagged Album",
            "-metadata",
            "genre=Jazz",
            "-metadata",
            "composer=Composer Name",
            "-metadata",
            "performer=Guest Name",
            "-metadata",
            "track=3/12",
            "-metadata",
            "disc=2/2",
            "-metadata",
            "date=2024-02-03",
            "-metadata",
            "comment=Tagged comment",
        ])
        .arg(&audio)
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let direct = probe_media(&audio).expect("tagged FLAC probe");
    assert_eq!(
        direct.tags.artist.as_deref(),
        Some("Track Artist"),
        "{direct:?}"
    );
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::audio_only(),
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let item = catalog
        .items
        .values()
        .find(|item| item.path == audio)
        .expect("tagged audio item");
    assert_eq!(item.title, "Tagged Song");
    assert_eq!(item.artist.as_deref(), Some("Track Artist"));
    assert_eq!(item.album_artist.as_deref(), Some("Album Artist"));
    assert_eq!(item.album.as_deref(), Some("Tagged Album"));
    assert_eq!(item.genre.as_deref(), Some("Jazz"));
    assert_eq!(item.composer.as_deref(), Some("Composer Name"));
    assert_eq!(item.contributor.as_deref(), Some("Guest Name"));
    assert_eq!(item.track, Some(3));
    assert_eq!(item.disc, Some(2));
    assert!(item.date.starts_with("2024-02-03"), "{}", item.date);
    for (root_id, title) in [
        (MUSIC_ARTIST_ID, "Track Artist"),
        (MUSIC_ALBUM_ARTIST_ID, "Album Artist"),
        (MUSIC_ALBUM_ID, "Tagged Album"),
        (MUSIC_GENRE_ID, "Jazz"),
        (MUSIC_COMPOSER_ID, "Composer Name"),
        (MUSIC_CONTRIB_ARTIST_ID, "Guest Name"),
    ] {
        let children = catalog.children_of(root_id).unwrap();
        assert!(
            children.iter().any(|child| matches!(
                child,
                CatalogChild::Container(container) if container.title == title
            )),
            "missing {title} below {root_id}"
        );
    }

    let nfo = root.join("tagged.nfo");
    std::fs::write(&nfo, "<musicvideo><title>Sidecar Wins</title></musicvideo>").unwrap();
    let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&nfo)).unwrap();
    assert_eq!(delta.changed, 1);
    let updated = updated.unwrap();
    let item = updated
        .items
        .values()
        .find(|item| item.path == audio)
        .expect("updated tagged audio item");
    assert_eq!(item.title, "Sidecar Wins");
    assert_eq!(item.artist.as_deref(), Some("Track Artist"));
    std::fs::write(&nfo, "<musicvideo><title>Sidecar Wins</title><genre>Rock</genre><showtitle>Override Album</showtitle><studio>Studio</studio><season>9</season><episode>8</episode><year>2001</year></musicvideo>").unwrap();
    monitor(&cfg).unwrap();
    std::fs::write(&nfo, "<musicvideo><title>Sidecar Wins</title></musicvideo>").unwrap();
    let (restored, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let restored = restored.unwrap();
    let item = restored
        .items
        .values()
        .find(|item| item.path == audio)
        .unwrap();
    assert_eq!(item.genre.as_deref(), Some("Jazz"));
    assert_eq!(item.album.as_deref(), Some("Tagged Album"));
    assert_eq!(item.creator, None);
    assert_eq!(item.disc, Some(2));
    assert_eq!(item.track, Some(3));
    assert!(item.date.starts_with("2024-02-03"));
    std::fs::remove_file(&nfo).unwrap();
    let (restored, delta) = monitor(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let restored = restored.unwrap();
    let item = restored
        .items
        .values()
        .find(|item| item.path == audio)
        .unwrap();
    assert_eq!(item.title, "Tagged Song");
    assert_eq!(item.artist.as_deref(), Some("Track Artist"));
    assert_eq!(item.album_artist.as_deref(), Some("Album Artist"));
    assert_eq!(item.composer.as_deref(), Some("Composer Name"));
    assert_eq!(item.contributor.as_deref(), Some("Guest Name"));
    assert!(monitor(&cfg).unwrap().0.is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn video_audio_track_title_never_replaces_filename_or_nfo_title() {
    let tmp = TempPath::new("video-track-title");
    let root = tmp.join("video");
    std::fs::create_dir_all(&root).unwrap();
    let video = root.join("Actual Movie Name.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=1",
            "-shortest",
            "-threads",
            "1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-metadata:s:a:0",
            "title=MVO [HDRezka Studio]",
            "-metadata",
            "title=Rip_by_M@kSIMus",
        ])
        .arg(&video)
        .stdin(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !status {
        eprintln!("skip video track-title policy (ffmpeg fixture unavailable)");
        return;
    }
    std::fs::write(
        video.with_extension("nfo"),
        "<movie><genre>Test Genre</genre></movie>",
    )
    .unwrap();
    let direct = probe_media(&video).expect("video probe");
    assert_eq!(direct.tags.title.as_deref(), Some("Rip_by_M@kSIMus"));

    let db_path = tmp.join("files.db");
    let cfg = ScanConfig {
        media_dirs: vec![root],
        db_path: Some(db_path.clone()),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let item = catalog
        .items
        .values()
        .find(|item| item.path == video)
        .expect("video item");
    assert_eq!(item.title, "Actual Movie Name");

    let db = LibraryDb::open(&db_path).unwrap();
    let id = db
        .find_detail_by_path(&path_to_db(&video))
        .unwrap()
        .unwrap()
        .id;
    db.update_detail_title(id, "MVO").unwrap();
    assert_eq!(
        db.update_detail_names_under_root(id, VIDEO_GENRE_ID, "Нарезка")
            .unwrap(),
        1
    );
    db.set_setting("video_title_policy_rev", "2").unwrap();
    drop(db);
    let (repaired, delta) = repair_video_titles_if_needed(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    let repaired = repaired.unwrap();
    assert_eq!(
        repaired
            .items
            .values()
            .find(|item| item.path == video)
            .unwrap()
            .title,
        "Actual Movie Name"
    );
    let genre = container_named(&repaired, VIDEO_GENRE_ID, "Test Genre").unwrap();
    assert_eq!(item_titles(&repaired, genre), vec!["Actual Movie Name"]);

    std::fs::write(
        video.with_extension("nfo"),
        "<movie><title>Curated NFO Name</title></movie>",
    )
    .unwrap();
    let db = LibraryDb::open(&db_path).unwrap();
    db.update_detail_title(id, "MVO").unwrap();
    db.set_setting("video_title_policy_rev", "0").unwrap();
    drop(db);
    let (repaired, delta) = repair_video_titles_if_needed(&cfg).unwrap();
    assert_eq!(delta.changed, 1);
    assert_eq!(
        repaired
            .unwrap()
            .items
            .values()
            .find(|item| item.path == video)
            .unwrap()
            .title,
        "Curated NFO Name"
    );
}

fn jpeg_with_test_exif(base_jpeg: &[u8]) -> Vec<u8> {
    let strings = [
        (0x010f_u16, b"TestCam\0".as_slice()),
        (0x0110_u16, b"Model X\0".as_slice()),
        (0x010e_u16, b"A photo\0".as_slice()),
        (0x0132_u16, b"2024:02:03 04:05:06\0".as_slice()),
        (0x010d_u16, b"Vacation\0".as_slice()),
    ];
    let entry_count = 9_u16;
    let data_start = 8 + 2 + usize::from(entry_count) * 12 + 4;
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42_u16.to_le_bytes());
    tiff.extend_from_slice(&8_u32.to_le_bytes());
    tiff.extend_from_slice(&entry_count.to_le_bytes());
    let mut tail = Vec::new();
    for (tag, value) in strings {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&2_u16.to_le_bytes());
        tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
        let offset = (data_start + tail.len()) as u32;
        tiff.extend_from_slice(&offset.to_le_bytes());
        tail.extend_from_slice(value);
    }
    for (tag, value) in [(0x0100_u16, 2_u32), (0x0101_u16, 4_u32)] {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&4_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    }
    tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
    tiff.extend_from_slice(&3_u16.to_le_bytes());
    tiff.extend_from_slice(&1_u32.to_le_bytes());
    tiff.extend_from_slice(&6_u16.to_le_bytes());
    tiff.extend_from_slice(&0_u16.to_le_bytes());
    tiff.extend_from_slice(&0x4746_u16.to_le_bytes());
    tiff.extend_from_slice(&3_u16.to_le_bytes());
    tiff.extend_from_slice(&1_u32.to_le_bytes());
    tiff.extend_from_slice(&4_u16.to_le_bytes());
    tiff.extend_from_slice(&0_u16.to_le_bytes());
    let ifd1_offset = (data_start + tail.len()) as u32;
    tiff.extend_from_slice(&ifd1_offset.to_le_bytes());
    tiff.extend_from_slice(&tail);
    let thumbnail_offset = ifd1_offset + 2 + 2 * 12 + 4;
    tiff.extend_from_slice(&2_u16.to_le_bytes());
    for (tag, value) in [
        (0x0201_u16, thumbnail_offset),
        (0x0202_u16, TINY_JPEG.len() as u32),
    ] {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&4_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    }
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(TINY_JPEG);

    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut jpeg = base_jpeg.to_vec();
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(&app1);
    jpeg.splice(2..2, segment);
    jpeg
}

#[test]
fn jpeg_exif_populates_oriented_metadata_and_image_views() {
    let tmp = TempPath::new("image-exif");
    let _ = std::fs::remove_dir_all(&tmp);
    let root = tmp.join("pictures");
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("photo.jpg");
    let base = tmp.join("base.jpg");
    write_solid_jpeg(&base, 2, 4);
    std::fs::write(&image, jpeg_with_test_exif(&std::fs::read(&base).unwrap())).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes {
            video: false,
            audio: false,
            image: true,
        },
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let item = catalog
        .items
        .values()
        .find(|item| item.path == image)
        .expect("EXIF image");
    assert_eq!(item.title, "A photo");
    assert_eq!(item.about.as_deref(), Some("A photo"));
    assert_eq!(item.creator.as_deref(), Some("TestCam Model X"));
    assert_eq!(item.date, "2024-02-03T04:05:06Z");
    assert_eq!(item.rotation, Some(90));
    assert_eq!(item.album.as_deref(), Some("Vacation"));
    assert_eq!(item.rating, Some(4));
    assert_eq!(item.resolution.as_deref(), Some("4x2"));
    assert!(item.album_art > 0, "EXIF thumbnail should become album art");
    assert_eq!(
        std::fs::read(catalog.album_art_paths.get(&item.album_art).unwrap()).unwrap(),
        TINY_JPEG
    );
    for (root_id, title) in [
        (IMAGE_DATE_ID, "2024-02-03"),
        (IMAGE_CAMERA_ID, "TestCam Model X"),
        (IMAGE_ALBUM_ID, "Vacation"),
        (IMAGE_RATING_ID, "4"),
    ] {
        let children = catalog.children_of(root_id).unwrap();
        assert!(children.iter().any(|child| matches!(
            child,
            CatalogChild::Container(container) if container.title == title
        )));
    }
    let resized = tmp.join("resized.jpg");
    assert!(scale_jpeg_result(&image, &resized, 20, 20).unwrap());
    let resized_probe = probe_image(&resized).unwrap();
    assert_eq!(
        (resized_probe.probe.width, resized_probe.probe.height),
        (20, 10),
        "resized output must apply EXIF orientation"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn playlists_parse_refresh_and_keep_ids_across_rename() {
    let tmp = TempPath::new("playlists");
    let _ = std::fs::remove_dir_all(&tmp);
    let audio_root = tmp.join("audio");
    let video_root = tmp.join("video");
    let image_root = tmp.join("pictures");
    for root in [&audio_root, &video_root, &image_root] {
        std::fs::create_dir_all(root).unwrap();
    }
    let song = audio_root.join("café.flac");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=0.1",
            "-c:a",
            "flac",
        ])
        .arg(&song)
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let movie = video_root.join("movie.mkv");
    write_fake_mkv(&movie, 4096);
    let picture = image_root.join("photo.jpg");
    std::fs::write(&picture, TINY_JPEG).unwrap();
    let outside = tmp.join("outside.mkv");
    write_fake_mkv(&outside, 4096);

    let playlist = audio_root.join("Mixed.m3u");
    let mut latin1 = b"#EXTM3U\ncaf".to_vec();
    latin1.push(0xe9);
    latin1.extend_from_slice(
            b".flac\n../video/movie.mkv\n../video/movie.mkv\n../pictures/photo.jpg\n../outside.mkv\nmissing.mp3\n",
        );
    std::fs::write(&playlist, latin1).unwrap();
    std::fs::write(
        audio_root.join("Ordered.pls"),
        "[playlist]\nFile2=../video/movie.mkv\nFile1=café.flac\nNumberOfEntries=2\n",
    )
    .unwrap();
    std::fs::write(audio_root.join("bad.m3u8"), [0xff, 0xfe]).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![audio_root.clone(), video_root.clone(), image_root.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::all(),
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let roots = [MUSIC_PLIST_ID, VIDEO_PLIST_ID, IMAGE_PLIST_ID];
    for root in roots {
        let children = catalog.children_of(root).unwrap();
        assert!(
            children.iter().any(|child| matches!(
                child,
                CatalogChild::Container(container) if container.title == "Mixed"
            )),
            "missing Mixed below {root}"
        );
    }
    let mixed = catalog
        .children_of(VIDEO_PLIST_ID)
        .unwrap()
        .into_iter()
        .find_map(|child| match child {
            CatalogChild::Container(container) if container.title == "Mixed" => Some(container),
            _ => None,
        })
        .unwrap();
    let stable_id = mixed.object_id.clone();
    let members = catalog.children_of(&mixed.object_id).unwrap();
    assert_eq!(
        members.len(),
        2,
        "duplicate entries are ordered and preserved"
    );
    assert!(members.iter().all(|member| matches!(
        member,
        CatalogChild::Item(item) if item.path == movie
    )));

    let renamed = audio_root.join("Renamed.m3u");
    std::fs::rename(&playlist, &renamed).unwrap();
    let (updated, delta) = monitor(&cfg).unwrap();
    assert!(delta.changed >= 1);
    let updated = updated.unwrap();
    let renamed_container = updated
        .children_of(VIDEO_PLIST_ID)
        .unwrap()
        .into_iter()
        .find_map(|child| match child {
            CatalogChild::Container(container) if container.title == "Renamed" => Some(container),
            _ => None,
        })
        .unwrap();
    assert_eq!(renamed_container.object_id, stable_id);

    std::fs::write(&renamed, "../pictures/photo.jpg\n").unwrap();
    let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&renamed)).unwrap();
    assert!(delta.changed >= 1);
    let updated = updated.unwrap();
    assert!(!updated.children_of(VIDEO_PLIST_ID).unwrap().iter().any(
        |child| matches!(child, CatalogChild::Container(container) if container.title == "Renamed")
    ));
    assert!(updated.children_of(IMAGE_PLIST_ID).unwrap().iter().any(
        |child| matches!(child, CatalogChild::Container(container) if container.title == "Renamed")
    ));

    std::fs::remove_file(&renamed).unwrap();
    let (updated, delta) = monitor(&cfg).unwrap();
    assert!(delta.changed >= 1);
    let updated = updated.unwrap();
    assert!(!updated
        .containers
        .values()
        .any(|container| container.title == "Renamed"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn non_utf8_image_names_keep_still_image_probe_semantics_across_replacement() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempPath::new("nonutf8-image");
    let media = tmp.join("pictures");
    std::fs::create_dir_all(&media).unwrap();
    let mut name = b"photo-".to_vec();
    name.push(0x80);
    name.extend_from_slice(b".jpg");
    let image = media.join(OsString::from_vec(name));
    let first = tmp.join("first.jpg");
    write_solid_jpeg(&first, 4, 6);
    std::fs::copy(&first, &image).unwrap();

    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes {
            video: false,
            audio: false,
            image: true,
        },
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    let item = catalog
        .items
        .values()
        .find(|item| item.path == image)
        .unwrap();
    assert_eq!(item.resolution.as_deref(), Some("4x6"));
    assert_eq!(item.probe.container, "jpeg");
    assert!(item.probe.video.is_empty());

    let second = tmp.join("second.jpg");
    write_solid_jpeg(&second, 8, 4);
    std::fs::copy(&second, &image).unwrap();
    let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&image)).unwrap();
    assert_eq!(delta.changed, 1);
    let catalog = catalog.expect("image dimensions changed");
    let item = catalog
        .items
        .values()
        .find(|item| item.path == image)
        .unwrap();
    assert_eq!(item.resolution.as_deref(), Some("8x4"));
    assert_eq!(item.probe.container, "jpeg");
    assert!(item.probe.video.is_empty());
}

#[cfg(unix)]
#[test]
fn non_utf8_media_uses_configured_and_builtin_artwork_on_scan_and_dirty_refresh() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempPath::new("nonutf8-artwork");
    std::fs::create_dir_all(&tmp).unwrap();
    for (index, configured, sidecar_kind) in [
        (0, vec!["Shared-Art.jpg".to_string()], "exact"),
        (1, vec!["{stem}-custom.jpg".to_string()], "stem"),
        (2, Vec::new(), "builtin-jpeg"),
        (3, vec!["%s-percent.jpg".to_string()], "percent"),
    ] {
        let media_dir = tmp.join(format!("case-{index}"));
        std::fs::create_dir_all(&media_dir).unwrap();
        let mut stem_bytes = b"movie-".to_vec();
        stem_bytes.push(0x80 + index as u8);
        let stem = OsString::from_vec(stem_bytes);
        let mut media_name = stem.clone();
        media_name.push(".mkv");
        let media = media_dir.join(media_name);
        write_fake_mkv(&media, 64);
        let sidecar_name = match sidecar_kind {
            "exact" => OsString::from("sHaReD-aRt.JpG"),
            "stem" => {
                let mut name = stem.clone();
                name.push("-CuStOm.JpG");
                name
            }
            "builtin-jpeg" => {
                let mut name = stem.clone();
                name.push("-PoStEr.JpEg");
                name
            }
            "percent" => {
                let mut name = stem.clone();
                name.push("-PeRcEnT.JpG");
                name
            }
            _ => unreachable!(),
        };
        let sidecar = media_dir.join(sidecar_name);
        std::fs::write(&sidecar, TINY_JPEG).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![media_dir],
            db_path: Some(tmp.join(format!("case-{index}.db"))),
            types: MediaTypes::video_only(),
            album_art_names: configured,
            thumbnails: false,
            ..Default::default()
        };

        assert!(artwork_path_matches_media_with_names(
            &sidecar,
            &media,
            &cfg.album_art_names
        ));
        let catalog = scan(&cfg).unwrap();
        let item = catalog
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == media)
            .unwrap();
        assert!(item.album_art > 0);
        assert_eq!(catalog.album_art_paths.get(&item.album_art), Some(&sidecar));

        std::fs::remove_file(&sidecar).unwrap();
        let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&sidecar)).unwrap();
        assert_eq!(delta.changed, 1, "{sidecar_kind} deletion");
        let catalog = catalog.expect("artwork deletion changed catalog");
        assert!(catalog
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == media)
            .is_some_and(|item| item.album_art == 0));

        std::fs::write(&sidecar, TINY_JPEG).unwrap();
        let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&sidecar)).unwrap();
        assert_eq!(delta.changed, 1, "{sidecar_kind} recreation");
        let catalog = catalog.expect("artwork recreation changed catalog");
        let item = catalog
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == media)
            .unwrap();
        assert!(item.album_art > 0);
        assert_eq!(catalog.album_art_paths.get(&item.album_art), Some(&sidecar));
    }
}

#[cfg(unix)]
#[test]
fn album_art_case_variants_have_deterministic_candidate_and_name_precedence() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempPath::new("artwork-case-precedence");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut stem = b"movie-".to_vec();
    stem.push(0x80);
    let stem = OsString::from_vec(stem);
    let mut media_name = stem.clone();
    media_name.push(".mkv");
    let media = tmp.join(media_name);
    write_fake_mkv(&media, 64);
    let upper = tmp.join("SHARED-ART.JPG");
    let mixed = tmp.join("Shared-Art.JpG");
    std::fs::write(&upper, TINY_JPEG).unwrap();
    std::fs::write(&mixed, TINY_JPEG).unwrap();
    let cfg = ScanConfig {
        album_art_names: vec!["shared-art.jpg".into()],
        ..Default::default()
    };

    assert_eq!(
        find_album_art_for_config(&media, &cfg),
        Some(upper.clone()),
        "without an exact spelling, raw filename order breaks ASCII-CI ties"
    );
    let exact = tmp.join("shared-art.jpg");
    std::fs::write(&exact, TINY_JPEG).unwrap();
    assert_eq!(
        find_album_art_for_config(&media, &cfg),
        Some(exact),
        "the configured spelling wins before other ASCII-case variants"
    );
}

#[test]
fn large_directory_artwork_lookup_stays_indexed_and_never_rescans_per_media() {
    let mut inventory_files = Vec::new();
    for index in 0..10_000 {
        let name = OsString::from(format!("noise-{index:05}.bin"));
        inventory_files.push((name.clone(), PathBuf::from(name)));
    }
    inventory_files.push((
        OsString::from("SHARED-ART.JPG"),
        PathBuf::from("SHARED-ART.JPG"),
    ));
    let inventory = ArtworkInventory::new(inventory_files);
    let configured = vec!["shared-art.jpg".to_string()];
    for index in 0..2_000 {
        let media = PathBuf::from(format!("movie-{index:04}.mkv"));
        assert_eq!(
            find_album_art_in_inventory(&media, &configured, &inventory),
            Some(PathBuf::from("SHARED-ART.JPG"))
        );
    }
    assert_eq!(
        inventory.probe_count(),
        2_000,
        "one configured candidate is one indexed lookup regardless of directory size"
    );

    let tmp = TempPath::new("artwork-no-rescan");
    let media_dir = tmp.join("video");
    std::fs::create_dir_all(&media_dir).unwrap();
    let source = tmp.join("source.mkv");
    write_fake_mkv(&source, 64);
    for index in 0..128 {
        std::fs::hard_link(&source, media_dir.join(format!("movie-{index:03}.mkv"))).unwrap();
    }
    for index in 0..512 {
        std::fs::write(media_dir.join(format!("noise-{index:03}.bin")), []).unwrap();
    }
    let sidecar = media_dir.join("SHARED-ART.JPG");
    std::fs::write(&sidecar, TINY_JPEG).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media_dir.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        album_art_names: configured,
        thumbnails: false,
        ..Default::default()
    };

    reset_artwork_directory_read_count(&media_dir);
    let catalog = scan(&cfg).unwrap();
    assert_eq!(
        catalog
            .items
            .values()
            .filter(|item| item.ref_id.is_none() && item.path.extension() == Some(OsStr::new("mkv")))
            .count(),
        128
    );
    monitor(&cfg).unwrap();
    std::fs::remove_file(&sidecar).unwrap();
    let (_, delta) = monitor_dirty(&cfg, std::slice::from_ref(&sidecar)).unwrap();
    assert_eq!(delta.changed, 1);
    assert_eq!(
        take_artwork_directory_read_count(&media_dir),
        0,
        "initial, periodic, and targeted attachment must reuse one directory inventory"
    );
}

#[cfg(unix)]
#[test]
fn physical_target_artwork_inventory_is_cached_across_many_symlink_media() {
    const SYMLINK_COUNT: usize = SCAN_PREPARATION_BATCH_FILES + 8;

    let tmp = TempPath::new("artwork-target-cache");
    let aliases = tmp.join("aliases");
    let targets = tmp.join("targets");
    std::fs::create_dir_all(&aliases).unwrap();
    std::fs::create_dir_all(&targets).unwrap();
    let source = targets.join("source.mkv");
    write_fake_mkv(&source, 64);
    for index in 0..SYMLINK_COUNT {
        let target = targets.join(format!("target-{index:03}.mkv"));
        std::fs::copy(&source, &target).unwrap();
        std::os::unix::fs::symlink(target, aliases.join(format!("alias-{index:03}.mkv"))).unwrap();
    }
    std::fs::write(targets.join("SHARED-ART.JPG"), TINY_JPEG).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        exclude_dirs: vec!["targets".into()],
        types: MediaTypes::video_only(),
        album_art_names: vec!["shared-art.jpg".into()],
        thumbnails: false,
        ..Default::default()
    };

    reset_artwork_directory_read_count(&targets);
    let catalog = scan(&cfg).unwrap();
    assert_eq!(
        catalog
            .items
            .values()
            .filter(|item| item.ref_id.is_none() && item.path.starts_with(&aliases))
            .count(),
        SYMLINK_COUNT
    );
    monitor(&cfg).unwrap();
    assert_eq!(
        take_artwork_directory_read_count(&targets),
        2,
        "initial preparation and periodic reconciliation each read the shared target once"
    );
}

#[cfg(unix)]
#[test]
fn targeted_additions_and_rebuild_cache_local_and_physical_artwork_inventories() {
    const FILES_PER_DIRECTORY: usize = 24;

    let tmp = TempPath::new("artwork-index-pass-cache");
    let direct = tmp.join("direct");
    let aliases = tmp.join("aliases");
    let targets = tmp.join("targets");
    std::fs::create_dir_all(&direct).unwrap();
    std::fs::create_dir_all(&aliases).unwrap();
    std::fs::create_dir_all(&targets).unwrap();
    std::fs::write(direct.join("SHARED-ART.JPG"), TINY_JPEG).unwrap();
    std::fs::write(targets.join("SHARED-ART.JPG"), TINY_JPEG).unwrap();
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![tmp.clone()],
        db_path: Some(tmp.join("files.db")),
        exclude_dirs: vec!["targets".into()],
        types: MediaTypes::video_only(),
        album_art_names: vec!["shared-art.jpg".into()],
        thumbnails: false,
        ..Default::default()
    };

    scan(&cfg).unwrap();
    let mut dirty = Vec::new();
    for index in 0..FILES_PER_DIRECTORY {
        let path = direct.join(format!("direct-{index:03}.mkv"));
        write_fake_mkv(&path, 64);
        dirty.push(path);

        let target = targets.join(format!("target-{index:03}.mkv"));
        write_fake_mkv(&target, 64);
        let alias = aliases.join(format!("alias-{index:03}.mkv"));
        std::os::unix::fs::symlink(target, &alias).unwrap();
        dirty.push(alias);
    }

    for dir in [&direct, &aliases, &targets] {
        reset_artwork_directory_read_count(dir);
    }
    let (_, delta) = monitor_dirty(&cfg, &dirty).unwrap();
    assert_eq!(delta.added, FILES_PER_DIRECTORY * 2);
    for (dir, role) in [
        (&direct, "direct media parent"),
        (&aliases, "symlink media parent"),
        (&targets, "canonical target parent"),
    ] {
        assert_eq!(
            take_artwork_directory_read_count(dir),
            1,
            "targeted additions must read the {role} once for the whole dirty batch"
        );
    }

    for dir in [&direct, &aliases, &targets] {
        reset_artwork_directory_read_count(dir);
    }
    let rebuilt = rebuild_objects(&cfg).unwrap();
    assert_eq!(
        rebuilt
            .items
            .values()
            .filter(|item| item.ref_id.is_none() && item.path.extension() == Some(OsStr::new("mkv")))
            .count(),
        FILES_PER_DIRECTORY * 2
    );
    for (dir, role) in [
        (&direct, "direct media parent"),
        (&aliases, "symlink media parent"),
        (&targets, "canonical target parent"),
    ] {
        assert_eq!(
            take_artwork_directory_read_count(dir),
            1,
            "object rebuild must read the {role} once for the whole pass"
        );
    }
}

#[cfg(unix)]
#[test]
fn direct_epoch_advance_rejects_malformed_and_out_of_range_values() {
    let tmp = TempPath::new("scan-direct-epoch-parse");
    let db = open_library_db(&tmp.join("files.db")).unwrap();
    for malformed in ["not-a-number", "18446744073709551616"] {
        db.set_setting(db::SCAN_CATALOG_EPOCH_KEY, malformed)
            .unwrap();
        assert!(matches!(
            advance_scan_catalog_epoch(&db),
            Err(ScanError::Database(
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, _)
            ))
        ));
        assert_eq!(
            db.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap().as_deref(),
            Some(malformed)
        );
    }
    db.set_scan_catalog_epoch(u64::MAX).unwrap();
    assert!(matches!(
        advance_scan_catalog_epoch(&db),
        Err(ScanError::Invariant(message)) if message.contains("epoch space")
    ));
    assert_eq!(
        db.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap().as_deref(),
        Some("18446744073709551615")
    );
}

#[test]
fn malformed_epoch_rolls_back_public_forget_without_losing_the_detail() {
    let tmp = TempPath::new("scan-forget-epoch-rollback");
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
    scan(&cfg).unwrap();
    let db_path = cfg.db_path.as_ref().unwrap();

    for malformed in ["not-a-number", "18446744073709551616"] {
        let db = open_library_db(db_path).unwrap();
        db.set_setting(db::SCAN_CATALOG_EPOCH_KEY, malformed)
            .unwrap();
        drop(db);
        assert!(matches!(
            forget_path(&cfg, &media),
            Err(ScanError::Database(
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, _)
            ))
        ));
        let db = open_library_db(db_path).unwrap();
        assert!(
            db.find_detail_by_path(&path_to_db(&media))
                .unwrap()
                .is_some(),
            "forget must roll back its row deletion when epoch validation fails"
        );
        assert_eq!(
            db.setting(db::SCAN_CATALOG_EPOCH_KEY).unwrap().as_deref(),
            Some(malformed)
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_survive_scan_restart_caption_and_rename() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let tmp = TempPath::new("nonutf8");
    let media = tmp.join("video");
    std::fs::create_dir_all(&media).unwrap();
    let raw_path = |byte: u8| {
        let mut name = b"clip-".to_vec();
        name.push(byte);
        name.extend_from_slice(b".mkv");
        media.join(OsString::from_vec(name))
    };
    let first = raw_path(0x80);
    let second = raw_path(0x81);
    write_fake_mkv(&first, 64);
    write_fake_mkv(&second, 64);
    let mut caption_name = first.file_stem().unwrap().as_bytes().to_vec();
    caption_name.extend_from_slice(b".en.srt");
    let caption = media.join(OsString::from_vec(caption_name));
    std::fs::write(&caption, "1\n00:00:00,000 --> 00:00:01,000\nhello\n").unwrap();

    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![media.clone()],
        db_path: Some(tmp.join("files.db")),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let lossy_first = first.file_name().unwrap().to_string_lossy();
    assert!(is_video(&lossy_first), "media classifier: {lossy_first:?}");
    assert!(!is_unfinished_name(&lossy_first));
    assert!(!is_caption_name(&lossy_first));
    assert!(!is_album_art_name_for_config(&lossy_first, &cfg));
    assert!(!path_excluded(&first, &lossy_first, &cfg));
    assert!(cfg
        .root_types_for_path(&first)
        .is_some_and(|types| types.allows(&lossy_first)));
    assert!(path_is_allowed_file(&first, &cfg));
    assert!(file_is_viable(&first));
    let catalog = scan(&cfg).unwrap();
    let first_item = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == first)
        .expect("first invalid-byte path");
    let second_item = catalog
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == second)
        .expect("second invalid-byte path");
    let first_title = first_item.title.clone();
    let second_title = second_item.title.clone();
    assert_ne!(first_item.detail_id, second_item.detail_id);
    assert_ne!(first_item.title, second_item.title);
    assert_eq!(first_item.captions.len(), 1);
    assert_eq!(first_item.captions[0].path, caption);
    assert_eq!(first_item.captions[0].ext, "srt");
    assert_eq!(path_from_db(&path_to_db(&first)), first);
    assert_ne!(path_to_db(&first), path_to_db(&second));

    let restarted = load_existing(&cfg);
    assert!(restarted.items.values().any(|item| item.path == first));
    assert!(restarted.items.values().any(|item| item.path == second));
    assert!(restarted
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == first)
        .is_some_and(|item| item.captions.iter().any(|cap| cap.path == caption)));

    write_fake_mkv(&first, 96);
    let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&first)).unwrap();
    assert_eq!(delta.changed, 1);
    let updated = updated.expect("same-path replacement changed stream metadata");
    let replaced = updated
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == first)
        .unwrap();
    let untouched = updated
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == second)
        .unwrap();
    assert_eq!(replaced.title, first_title);
    assert_eq!(untouched.title, second_title);
    assert_ne!(replaced.title, untouched.title);

    std::fs::write(&caption, "replacement subtitle bytes").unwrap();
    let (_, delta) = monitor_dirty(&cfg, std::slice::from_ref(&caption)).unwrap();
    assert_eq!(delta.changed, 1, "invalid-byte caption dirties its owner");

    let mut poster_name = first.file_stem().unwrap().as_bytes().to_vec();
    poster_name.extend_from_slice(b"-poster.jpg");
    let poster = media.join(OsString::from_vec(poster_name));
    std::fs::write(&poster, TINY_JPEG).unwrap();
    let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&poster)).unwrap();
    assert_eq!(delta.changed, 1, "invalid-byte artwork dirties its owner");
    let updated = updated.expect("artwork changed catalog");
    assert!(updated
        .items
        .values()
        .find(|item| item.ref_id.is_none() && item.path == first)
        .is_some_and(|item| item.album_art > 0));

    let mut nfo_name = first.file_stem().unwrap().as_bytes().to_vec();
    nfo_name.extend_from_slice(b".nfo");
    let nfo = media.join(OsString::from_vec(nfo_name));
    std::fs::write(&nfo, "<movie><title>Raw sidecar title</title></movie>").unwrap();
    let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&nfo)).unwrap();
    assert_eq!(delta.changed, 1, "invalid-byte NFO dirties its owner");
    let updated = updated.expect("NFO changed catalog");
    assert!(updated.items.values().any(|item| {
        item.ref_id.is_none() && item.path == first && item.title == "Raw sidecar title"
    }));

    let renamed = raw_path(0x82);
    std::fs::rename(&first, &renamed).unwrap();
    let (updated, delta) = monitor_dirty(&cfg, &[first.clone(), renamed.clone()]).unwrap();
    assert!(delta.added >= 1 && delta.removed >= 1, "{delta:?}");
    let updated = updated.expect("catalog changed");
    assert!(!updated.items.values().any(|item| item.path == first));
    assert!(updated.items.values().any(|item| item.path == second));
    assert!(updated.items.values().any(|item| item.path == renamed));

    let reserved = PathBuf::from(format!("{PATH_HEX_PREFIX}ordinary"));
    assert_eq!(path_from_db(&path_to_db(&reserved)), reserved);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn malformed_exif_date_keeps_image_probe_and_scan_usable() {
    let temp = TempPath::new("malformed-exif-date");
    std::fs::create_dir_all(&temp).unwrap();
    let image = temp.join("photo.jpg");
    write_solid_jpeg(&image, 2, 4);
    let mut jpeg = jpeg_with_test_exif(&std::fs::read(&image).unwrap());
    let date = b"2024:02:03 04:05:06";
    let offset = jpeg
        .windows(date.len())
        .position(|bytes| bytes == date)
        .unwrap();
    jpeg[offset + 18] = 0xff;
    std::fs::write(&image, jpeg).unwrap();
    let probed = probe_image(&image).expect("usable malformed-metadata JPEG");
    assert_eq!(probed.tags.date, None);
    assert_eq!(probed.tags.camera_make.as_deref(), Some("TestCam"));
    let cfg = ScanConfig {
        media_dirs: vec![temp.clone()],
        db_path: Some(temp.join("files.db")),
        types: MediaTypes {
            video: false,
            audio: false,
            image: true,
        },
        thumbnails: false,
        ..Default::default()
    };
    let catalog = scan(&cfg).unwrap();
    assert!(catalog.items.values().any(|item| item.path == image));
}

#[test]
fn recursive_nfo_refresh_obeys_precancellation() {
    let temp = TempPath::new("nfo-precancelled");
    std::fs::create_dir_all(&temp).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![temp.clone()],
        ..Default::default()
    };
    let db = LibraryDb::open_memory().unwrap();
    cfg.cancellation.cancel();
    assert!(matches!(
        apply_nfo_in_dir(&db, &cfg, &temp, true),
        Err(ScanError::Cancelled)
    ));
}

#[cfg(unix)]
#[test]
fn recursive_nfo_refresh_prunes_multiple_ancestor_cycles() {
    let temp = TempPath::new("nfo-recursive-cycles");
    std::fs::create_dir_all(temp.join("show/season")).unwrap();
    for name in ["loop1", "loop2"] {
        std::os::unix::fs::symlink(".", temp.join(name)).unwrap();
    }
    std::os::unix::fs::symlink("../..", temp.join("show/season/parent")).unwrap();
    let cfg = ScanConfig {
        media_dirs: vec![temp.clone()],
        ..Default::default()
    };
    let db = LibraryDb::open_memory().unwrap();
    let start = std::time::Instant::now();
    assert!(!apply_nfo_in_dir(&db, &cfg, &temp, true).unwrap());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn periodic_nfo_removal_converges_with_targeted_and_clean_scan() {
    #[derive(Debug, PartialEq, Eq)]
    struct NfoSnapshot {
        details: Vec<(String, db::DetailPresentation)>,
        groups: Vec<(PathBuf, Vec<String>, String)>,
    }
    fn snapshot(cfg: &ScanConfig) -> NfoSnapshot {
        let db = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
        let mut details = db
            .all_detail_stats()
            .unwrap()
            .into_iter()
            .map(|row| (row.path, db.detail_presentation(row.id).unwrap()))
            .collect::<Vec<_>>();
        details.sort_by(|left, right| left.0.cmp(&right.0));
        let catalog = db.load_catalog().unwrap();
        let mut groups = Vec::new();
        for item in catalog.items.values().filter(|item| {
            [VIDEO_SERIES_ID, VIDEO_GENRE_ID, VIDEO_ACTOR_ID]
                .iter()
                .any(|root| item.parent_id.starts_with(root))
        }) {
            let mut ancestors = Vec::new();
            let mut parent = item.parent_id.as_str();
            while let Some(container) = catalog.containers.get(parent) {
                ancestors.push(container.title.clone());
                parent = &container.parent_id;
            }
            groups.push((item.path.clone(), ancestors, item.title.clone()));
        }
        groups.sort();
        NfoSnapshot { details, groups }
    }
    for (failed_probe, sidecar_on_alias) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let temp = TempPath::new("periodic-nfo-removal");
        let root = temp.join("media");
        std::fs::create_dir_all(root.join("z-alias")).unwrap();
        let media = root.join("episode.mkv");
        let alias = root.join("z-alias/alternate.mkv");
        if failed_probe {
            let mut bytes = vec![0_u8; 64];
            bytes[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
            std::fs::write(&media, bytes).unwrap();
            assert!(probe_media(&media).is_none());
        } else {
            write_fake_mkv(&media, 64);
        }
        std::fs::hard_link(&media, &alias).unwrap();
        let sidecar = if sidecar_on_alias { &alias } else { &media }.with_extension("nfo");
        let full = "<episodedetails><title>Episode</title><showtitle>Show</showtitle><genre>Drama</genre><studio>Studio</studio><season>2</season><episode>3</episode><aired>2020-02-03</aired></episodedetails>";
        std::fs::write(&sidecar, full).unwrap();
        let gate = std::sync::Arc::new(HelperGate::new(1, 8));
        let periodic = ScanConfig {
            media_dirs: vec![root.clone()],
            db_path: Some(temp.join("periodic.db")),
            types: MediaTypes::video_only(),
            thumbnails: false,
            helper_gate: Some(gate.clone()),
            ..Default::default()
        };
        let mut targeted = periodic.clone();
        targeted.db_path = Some(temp.join("targeted.db"));
        let initial = scan(&periodic).unwrap();
        assert!(initial
            .items
            .values()
            .any(|item| item.path == media && item.genre.as_deref() == Some("Drama")));
        scan(&targeted).unwrap();
        let probes = gate.metrics().admitted_total;
        assert!(monitor(&periodic).unwrap().0.is_none());
        assert_eq!(
            gate.metrics().admitted_total,
            probes,
            "source-only NFO provenance is shared with its fresh alias"
        );
        let tags = [
            "<genre>Drama</genre>",
            "<showtitle>Show</showtitle>",
            "<studio>Studio</studio>",
            "<season>2</season>",
            "<episode>3</episode>",
            "<aired>2020-02-03</aired>",
        ];
        let mut variants = tags
            .iter()
            .map(|tag| Some(full.replace(tag, "")))
            .collect::<Vec<_>>();
        variants.push(Some(
            "<episodedetails><genre>Drama</genre></episodedetails>".into(),
        ));
        variants.push(Some(
            "<episodedetails><title>Episode</title></episodedetails>".into(),
        ));
        variants.push(None);
        for (index, next) in variants.iter().enumerate() {
            if let Some(next) = next {
                std::fs::write(&sidecar, next).unwrap();
            } else {
                std::fs::remove_file(&sidecar).unwrap();
            }
            let probes = gate.metrics().admitted_total;
            let (_, periodic_delta) = monitor(&periodic).unwrap();
            assert_eq!(
                gate.metrics().admitted_total - probes,
                1,
                "one physical reconstruction per change"
            );
            let (_, targeted_delta) =
                monitor_dirty(&targeted, std::slice::from_ref(&sidecar)).unwrap();
            assert_eq!(periodic_delta.changed, 1);
            assert_eq!(targeted_delta.changed, 1);
            let mut clean = periodic.clone();
            clean.db_path = Some(temp.join(format!("clean-{index}.db")));
            scan(&clean).unwrap();
            assert_eq!(
                snapshot(&periodic),
                snapshot(&targeted),
                "targeted versus periodic variant {index}"
            );
            assert_eq!(
                snapshot(&periodic),
                snapshot(&clean),
                "periodic versus clean variant {index}"
            );
            let probes = gate.metrics().admitted_total;
            let db = open_library_db(periodic.db_path.as_ref().unwrap()).unwrap();
            let generation = db.get_update_id().unwrap();
            let (catalog, delta) = monitor(&periodic).unwrap();
            assert!(catalog.is_none());
            assert_eq!(delta.changed, 0);
            assert_eq!(
                gate.metrics().admitted_total,
                probes,
                "unchanged NFO cannot reopen media"
            );
            assert_eq!(db.get_update_id().unwrap(), generation);
        }
        // A pre-fingerprint catalog may retain removed overrides even though
        // every sidecar is now absent. Reconstruct its unknown provenance once.
        let expected = snapshot(&periodic);
        let db = open_library_db(periodic.db_path.as_ref().unwrap()).unwrap();
        db.connection().execute("UPDATE DETAILS SET NFO_FINGERPRINT=NULL, GENRE='Legacy Genre', ALBUM='Legacy Show'", []).unwrap();
        let probes = gate.metrics().admitted_total;
        monitor(&periodic).unwrap();
        assert_eq!(gate.metrics().admitted_total - probes, 1);
        assert_eq!(snapshot(&periodic), expected);
        let probes = gate.metrics().admitted_total;
        assert!(monitor(&periodic).unwrap().0.is_none());
        assert_eq!(gate.metrics().admitted_total, probes);
        // Invalid XML cannot replace either presentation or recorded provenance.
        let before = snapshot(&periodic);
        std::fs::write(&sidecar, "<movie><title>broken</movie>").unwrap();
        assert!(monitor(&periodic).is_err());
        assert_eq!(snapshot(&periodic), before);
    }
}

#[cfg(unix)]
#[test]
fn recursive_nfo_events_preserve_aliases_and_cancel_staged_changes() {
    let temp = TempPath::new("nfo-recursive-events");
    let root = temp.join("media");
    let show = root.join("show");
    std::fs::create_dir_all(show.join("season")).unwrap();
    write_fake_mkv(&show.join("season/episode.mkv"), 64);
    std::os::unix::fs::symlink("show", root.join("alias-a")).unwrap();
    std::os::unix::fs::symlink("show", root.join("alias-b")).unwrap();
    for name in ["loop-a", "loop-b"] {
        std::os::unix::fs::symlink(".", root.join(name)).unwrap();
    }
    std::os::unix::fs::symlink("..", show.join("parent")).unwrap();
    let gate = std::sync::Arc::new(HelperGate::new(1, 8));
    let mut cfg = ScanConfig {
        media_dirs: vec![root.clone()],
        db_path: Some(temp.join("files.db")),
        types: MediaTypes::video_only(),
        thumbnails: false,
        helper_gate: Some(gate.clone()),
        ..Default::default()
    };
    scan(&cfg).unwrap();
    let nfo = root.join("tvshow.nfo");
    std::fs::write(
        &nfo,
        "<tvshow><title>Inherited Show</title><genre>Drama</genre></tvshow>",
    )
    .unwrap();
    let probes = gate.metrics().admitted_total;
    let start = std::time::Instant::now();
    let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&nfo)).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(delta.changed, 1);
    assert_eq!(
        gate.metrics().admitted_total - probes,
        1,
        "all non-cyclic aliases share one probe"
    );
    let catalog = catalog.unwrap();
    for prefix in ["show", "alias-a", "alias-b"] {
        assert!(
            catalog.items.values().any(|item| item.path
                == root.join(prefix).join("season/episode.mkv")
                && item.genre.as_deref() == Some("Drama")),
            "missing alias {prefix}"
        );
    }
    let db = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
    let id = db.all_detail_stats().unwrap()[0].id;
    let before = db.detail_presentation(id).unwrap();
    let fingerprint = db.detail_nfo_fingerprint(id).unwrap();
    let epoch = db.scan_catalog_epoch().unwrap();
    std::fs::remove_file(&nfo).unwrap();
    // Cancel only after the walk has reached its first reconstruction and
    // queued for the held helper permit, so this is mid-walk cancellation.
    let permit = gate.try_acquire().unwrap();
    let cancellation = cfg.cancellation.clone();
    let cancel_gate = gate.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while cancel_gate.metrics().queued == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "walk did not reach reconstruction"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        cancellation.cancel();
    });
    let start = std::time::Instant::now();
    assert!(monitor_dirty(&cfg, std::slice::from_ref(&nfo)).is_err());
    canceller.join().unwrap();
    drop(permit);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(db.detail_presentation(id).unwrap(), before);
    assert_eq!(db.detail_nfo_fingerprint(id).unwrap(), fingerprint);
    assert_eq!(db.scan_catalog_epoch().unwrap(), epoch);
    cfg.cancellation = CancellationToken::default();
    let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&nfo)).unwrap();
    assert_eq!(delta.changed, 1);
    assert!(catalog
        .unwrap()
        .items
        .values()
        .all(|item| item.genre.is_none()));
}
