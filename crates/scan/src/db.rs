//! rustyDLNA-compatible SQLite store (`scanner_sqlite.h`).
//!
//! Tables: OBJECTS, DETAILS, ALBUM_ART, CAPTIONS, BOOKMARKS, PLAYLISTS,
//! SETTINGS. WAL. On-disk file is `{db_dir}/files.db` (same name The dialect uses).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, params_from_iter, types::Value, Connection, OpenFlags, OptionalExtension};
use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALBUM_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, IMAGE_RECENT_ID, MUSIC_ALBUM_ARTIST_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID,
    MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, MUSIC_RECENT_ID,
    ROOT_ID, SAMSUNG_AUDIO, SAMSUNG_IMAGE, SAMSUNG_VIDEO, VIDEO_ACTOR_ID, VIDEO_ALL_ID,
    VIDEO_DIR_ID, VIDEO_GENRE_ID, VIDEO_ID, VIDEO_PLIST_ID, VIDEO_RATING_ID, VIDEO_RECENT_ID,
    VIDEO_SERIES_ID,
};

use crate::{
    path_from_db, path_is_live_file, path_is_unwanted, Caption, Catalog, CatalogPatch, Container,
    EmbeddedTags, MediaItem, NfoMeta, ScanConfig,
};

#[derive(Clone, Debug)]
pub struct ObjectSnap {
    pub object_id: String,
    pub parent_id: String,
    pub class: String,
    pub detail_id: Option<i64>,
    pub name: Option<String>,
    pub ref_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub contributor: Option<String>,
    pub creator: Option<String>,
    pub date: Option<String>,
    pub rating: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailPresentation {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub contributor: Option<String>,
    pub comment: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
    pub date: Option<String>,
    pub rating: Option<i64>,
    pub rotation: Option<i64>,
    pub album_art: i64,
    pub captions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistDbRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub timestamp: i64,
    pub device: i64,
    pub inode: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailStat {
    pub path: String,
    pub id: i64,
    pub size: i64,
    pub timestamp: i64,
    pub device: i64,
    pub inode: i64,
}

fn media_item_from_catalog_row(
    row: &rusqlite::Row<'_>,
    captions: &HashMap<i64, Vec<Caption>>,
) -> rusqlite::Result<MediaItem> {
    let object_id: String = row.get(0)?;
    let parent_id: String = row.get(1)?;
    let class: String = row.get(2)?;
    let object_name: Option<String> = row.get(3)?;
    let detail_id: i64 = row.get(4)?;
    let ref_id: Option<String> = row.get(5)?;
    let path = path_from_db(&row.get::<_, Option<String>>(6)?.unwrap_or_default());
    let mime = row
        .get::<_, Option<String>>(10)?
        .unwrap_or_else(|| "video/x-matroska".into());
    let ext = mime_to_ext(&mime);
    let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let detail_title: Option<String> = row.get(25)?;
    let under_series_or_genre = parent_id == VIDEO_SERIES_ID
        || parent_id.starts_with(&format!("{VIDEO_SERIES_ID}$"))
        || parent_id == VIDEO_GENRE_ID
        || parent_id.starts_with(&format!("{VIDEO_GENRE_ID}$"));
    let title = if under_series_or_genre {
        nonempty(object_name.clone()).or_else(|| nonempty(detail_title.clone()))
    } else {
        nonempty(detail_title).or_else(|| nonempty(object_name))
    }
    .unwrap_or_else(|| {
        path.file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("item")
            .to_string()
    });
    let container: Option<String> = row.get(19)?;
    let video: Option<String> = row.get(20)?;
    let audio: Option<String> = row.get(21)?;
    let audio_streams: Option<String> = row.get(22)?;
    let hdr: Option<String> = row.get(23)?;
    let resolution: Option<String> = row.get(16)?;
    let probe = crate::probe_from_stored(
        ext,
        container.as_deref(),
        video.as_deref(),
        audio.as_deref(),
        audio_streams.as_deref(),
        hdr.as_deref(),
        resolution.as_deref(),
    );
    let dlna_pn = row
        .get::<_, Option<String>>(11)?
        .filter(|value| !value.is_empty())
        .or_else(|| {
            crate::dlna_pn_from_probe(
                &probe.container,
                &probe.video,
                &probe.audio,
                &probe.hdr,
                probe.width,
                probe.height,
            )
        });
    Ok(MediaItem {
        object_id,
        parent_id,
        detail_id,
        title,
        class,
        date: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        path,
        mime,
        ext: ext.into(),
        size: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u64,
        mtime: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
        captions: captions.get(&detail_id).cloned().unwrap_or_default(),
        probe,
        dlna_pn,
        ref_id,
        device: row.get::<_, Option<i64>>(12)?.unwrap_or(0) as u64,
        inode: row.get::<_, Option<i64>>(13)?.unwrap_or(0) as u64,
        duration: row
            .get::<_, Option<String>>(14)?
            .filter(|value| !value.is_empty()),
        bitrate: row.get(15)?,
        resolution: resolution.filter(|value| !value.is_empty()),
        channels: row.get(17)?,
        samplerate: row.get(18)?,
        album_art: row.get::<_, Option<i64>>(24)?.unwrap_or(0),
        creator: nonempty(row.get(26)?),
        comment: nonempty(row.get(30)?),
        artist: nonempty(row.get(27)?),
        album_artist: nonempty(row.get(33)?),
        composer: nonempty(row.get(34)?),
        contributor: nonempty(row.get(35)?),
        album: nonempty(row.get(28)?),
        genre: nonempty(row.get(29)?),
        disc: row.get(31)?,
        track: row.get(32)?,
        rating: row.get(36)?,
        rotation: row.get(37)?,
        bookmark_sec: 0,
        watch_count: 0,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoDetailTitle {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub device: i64,
    pub inode: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExistingDetail {
    pub id: i64,
    pub size: i64,
    pub timestamp: i64,
    pub device: i64,
    pub inode: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeSource {
    pub id: i64,
    pub size: i64,
    pub timestamp: i64,
    pub path: String,
    pub mime: String,
    pub stream_probe_rev: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct NewDetail<'a> {
    pub path: &'a str,
    pub size: i64,
    pub timestamp: i64,
    pub title: &'a str,
    pub date: &'a str,
    pub mime: &'a str,
    pub device: i64,
    pub inode: i64,
    pub dlna_pn: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DetailStreamUpdate<'a> {
    pub duration: Option<&'a str>,
    pub bitrate: Option<i64>,
    pub resolution: Option<&'a str>,
    pub channels: Option<i64>,
    pub samplerate: Option<i64>,
    pub container: Option<&'a str>,
    pub video: Option<&'a str>,
    pub audio: Option<&'a str>,
    pub hdr: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailGroupFields {
    pub album: Option<String>,
    pub genre: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
    pub title: Option<String>,
    pub device: i64,
    pub inode: i64,
}

/// rustyDLNA schema from `SQLite schema`.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS OBJECTS (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  OBJECT_ID TEXT UNIQUE NOT NULL,
  PARENT_ID TEXT NOT NULL,
  REF_ID TEXT DEFAULT NULL,
  CLASS TEXT NOT NULL,
  DETAIL_ID INTEGER DEFAULT NULL REFERENCES DETAILS(ID) ON DELETE CASCADE,
  NAME TEXT DEFAULT NULL
);
CREATE TABLE IF NOT EXISTS DETAILS (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  PATH TEXT DEFAULT NULL,
  SIZE INTEGER,
  TIMESTAMP INTEGER,
  TITLE TEXT COLLATE NOCASE,
  DURATION TEXT,
  BITRATE INTEGER,
  SAMPLERATE INTEGER,
  CREATOR TEXT COLLATE NOCASE,
  ARTIST TEXT COLLATE NOCASE,
  ALBUM_ARTIST TEXT COLLATE NOCASE,
  COMPOSER TEXT COLLATE NOCASE,
  CONTRIBUTOR TEXT COLLATE NOCASE,
  ALBUM TEXT COLLATE NOCASE,
  GENRE TEXT COLLATE NOCASE,
  COMMENT TEXT,
  CHANNELS INTEGER,
  DISC INTEGER,
  TRACK INTEGER,
  RATING INTEGER,
  DATE DATE,
  RESOLUTION TEXT,
  THUMBNAIL BOOL DEFAULT 0,
  ALBUM_ART INTEGER DEFAULT NULL REFERENCES ALBUM_ART(ID) ON DELETE SET NULL,
  ROTATION INTEGER,
  DLNA_PN TEXT,
  MIME TEXT,
  DEVICE INTEGER,
  INODE INTEGER,
  STREAM_PROBE_REV INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS ALBUM_ART (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  PATH TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS CAPTIONS (
  ID INTEGER NOT NULL REFERENCES DETAILS(ID) ON DELETE CASCADE,
  PATH TEXT NOT NULL,
  PRIMARY KEY (ID, PATH)
);
CREATE TABLE IF NOT EXISTS BOOKMARKS (
  ID INTEGER PRIMARY KEY REFERENCES DETAILS(ID) ON DELETE CASCADE,
  SEC INTEGER,
  WATCH_COUNT INTEGER,
  UPDATED_AT INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS PLAYLISTS (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  NAME TEXT NOT NULL,
  PATH TEXT NOT NULL,
  ITEMS INTEGER DEFAULT 0,
  FOUND INTEGER DEFAULT 0,
  TIMESTAMP INTEGER DEFAULT 0,
  DEVICE INTEGER DEFAULT 0,
  INODE INTEGER DEFAULT 0
);
CREATE TABLE IF NOT EXISTS PLAYLIST_ITEMS (
  PLAYLIST_ID INTEGER NOT NULL REFERENCES PLAYLISTS(ID) ON DELETE CASCADE,
  DETAIL_ID INTEGER NOT NULL REFERENCES DETAILS(ID) ON DELETE CASCADE,
  POSITION INTEGER NOT NULL,
  PRIMARY KEY (PLAYLIST_ID, POSITION)
);
CREATE TABLE IF NOT EXISTS SETTINGS (
  KEY TEXT NOT NULL UNIQUE,
  VALUE TEXT
);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_INODE ON DETAILS(DEVICE, INODE);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_PATH ON DETAILS(PATH);
CREATE INDEX IF NOT EXISTS IDX_OBJECTS_PARENT ON OBJECTS(PARENT_ID, NAME, OBJECT_ID);
CREATE INDEX IF NOT EXISTS IDX_OBJECTS_DETAIL ON OBJECTS(DETAIL_ID);
CREATE INDEX IF NOT EXISTS IDX_OBJECTS_CLASS ON OBJECTS(CLASS, OBJECT_ID);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_TITLE ON DETAILS(TITLE, ID);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_DATE ON DETAILS(DATE, ID);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_ALBUM ON DETAILS(ALBUM, ID);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_TRACK ON DETAILS(TRACK, ID);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogQueryField {
    Title,
    Creator,
    Date,
    Class,
    Artist,
    Genre,
    Album,
    Actor,
    Id,
    ParentId,
    RefId,
    Track,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogQueryOp {
    Contains(String),
    DoesNotContain(String),
    Equals(String),
    NotEquals(String),
    LessThan { value: String, inclusive: bool },
    GreaterThan { value: String, inclusive: bool },
    DerivedFrom(String),
    Exists(bool),
    Never,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogQueryClause {
    pub field: CatalogQueryField,
    pub op: CatalogQueryOp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogQuerySort {
    pub field: CatalogQueryField,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogDefaultOrder {
    FoldersFirst,
    ClassTitle,
    ClassDiscTrackTitle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogQuery {
    /// OR of AND groups, matching the ContentDirectory search grammar.
    pub groups: Vec<Vec<CatalogQueryClause>>,
    pub sort: Vec<CatalogQuerySort>,
    pub default_order: CatalogDefaultOrder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogQueryPage {
    pub object_ids: Vec<String>,
    pub total: u32,
    /// Total searchable OBJECTS rows, used to detect a catalog/DB generation
    /// mismatch before publishing a mixed response.
    pub population: u32,
}

const QUERY_TITLE: &str = "CASE \
    WHEN o.PARENT_ID = '2$E' OR o.PARENT_ID LIKE '2$E$%' \
      OR o.PARENT_ID = '2$9' OR o.PARENT_ID LIKE '2$9$%' \
    THEN COALESCE(NULLIF(o.NAME, ''), NULLIF(d.TITLE, ''), '') \
    ELSE COALESCE(NULLIF(d.TITLE, ''), NULLIF(o.NAME, ''), '') END";

const CATALOG_ITEM_SELECT: &str =
    "SELECT o.OBJECT_ID, o.PARENT_ID, o.CLASS, o.NAME, o.DETAIL_ID, o.REF_ID,
            d.PATH, d.SIZE, d.TIMESTAMP, d.DATE, d.MIME, d.DLNA_PN,
            d.DEVICE, d.INODE, d.DURATION, d.BITRATE, d.RESOLUTION,
            d.CHANNELS, d.SAMPLERATE, d.CONTAINER, d.VIDEO, d.AUDIO,
            d.AUDIO_STREAMS, d.HDR,
            d.ALBUM_ART, d.TITLE, d.CREATOR, d.ARTIST, d.ALBUM, d.GENRE,
            d.COMMENT, d.DISC, d.TRACK, d.ALBUM_ARTIST, d.COMPOSER,
            d.CONTRIBUTOR, d.RATING, d.ROTATION
     FROM OBJECTS o JOIN DETAILS d ON o.DETAIL_ID = d.ID";

fn catalog_field_sql(field: CatalogQueryField) -> &'static str {
    match field {
        CatalogQueryField::Title => QUERY_TITLE,
        CatalogQueryField::Creator => "COALESCE(d.CREATOR, '')",
        CatalogQueryField::Date => "COALESCE(d.DATE, '')",
        CatalogQueryField::Class => "COALESCE(o.CLASS, '')",
        CatalogQueryField::Artist => "COALESCE(d.ARTIST, '')",
        CatalogQueryField::Genre => "COALESCE(d.GENRE, '')",
        CatalogQueryField::Album => "COALESCE(d.ALBUM, '')",
        // Actor metadata is not currently persisted in DETAILS, matching the
        // empty field exposed by the in-memory search row.
        CatalogQueryField::Actor => "''",
        CatalogQueryField::Id => "o.OBJECT_ID",
        CatalogQueryField::ParentId => "o.PARENT_ID",
        CatalogQueryField::RefId => "COALESCE(o.REF_ID, '')",
        CatalogQueryField::Track => "COALESCE(d.TRACK, 0)",
    }
}

fn full_class_sql() -> &'static str {
    "CASE WHEN o.CLASS LIKE 'object.%' THEN o.CLASS ELSE 'object.' || o.CLASS END"
}

fn catalog_clause_sql(clause: &CatalogQueryClause, values: &mut Vec<Value>) -> String {
    let field = catalog_field_sql(clause.field);
    match &clause.op {
        CatalogQueryOp::Contains(value) => {
            values.push(Value::Text(value.to_ascii_lowercase()));
            let field = if clause.field == CatalogQueryField::Class {
                full_class_sql()
            } else {
                field
            };
            format!("instr(lower({field}), ?) > 0")
        }
        CatalogQueryOp::DoesNotContain(value) => {
            values.push(Value::Text(value.to_ascii_lowercase()));
            format!("instr(lower({field}), ?) = 0")
        }
        CatalogQueryOp::Equals(value) => {
            values.push(Value::Text(value.clone()));
            format!("{field} = ? COLLATE NOCASE")
        }
        CatalogQueryOp::NotEquals(value) => {
            values.push(Value::Text(value.clone()));
            format!("{field} <> ? COLLATE NOCASE")
        }
        CatalogQueryOp::LessThan { value, inclusive } => {
            values.push(Value::Text(value.clone()));
            format!(
                "{field} {} ? COLLATE NOCASE",
                if *inclusive { "<=" } else { "<" }
            )
        }
        CatalogQueryOp::GreaterThan { value, inclusive } => {
            values.push(Value::Text(value.clone()));
            format!(
                "{field} {} ? COLLATE NOCASE",
                if *inclusive { ">=" } else { ">" }
            )
        }
        CatalogQueryOp::DerivedFrom(value) => {
            let value = if value.starts_with("object.") {
                value.clone()
            } else {
                format!("object.{value}")
            };
            values.push(Value::Text(value.clone()));
            values.push(Value::Text(value));
            let field = if clause.field == CatalogQueryField::Class {
                full_class_sql()
            } else {
                field
            };
            format!("(lower({field}) = lower(?) OR instr(lower({field}), lower(?) || '.') = 1)")
        }
        CatalogQueryOp::Exists(want) => {
            if *want {
                format!("{field} <> ''")
            } else {
                format!("{field} = ''")
            }
        }
        CatalogQueryOp::Never => "0".to_string(),
        CatalogQueryOp::All => "1".to_string(),
    }
}

fn catalog_order_sql(sort: &[CatalogQuerySort], default_order: CatalogDefaultOrder) -> String {
    let mut parts = Vec::new();
    if sort.is_empty() {
        match default_order {
            CatalogDefaultOrder::FoldersFirst => {
                parts.push("(o.DETAIL_ID IS NULL) DESC".to_string());
                parts.push(format!("{QUERY_TITLE} COLLATE NOCASE ASC"));
            }
            CatalogDefaultOrder::ClassTitle => {
                parts.push("o.CLASS COLLATE NOCASE ASC".to_string());
                parts.push(format!("{QUERY_TITLE} COLLATE NOCASE ASC"));
            }
            CatalogDefaultOrder::ClassDiscTrackTitle => {
                parts.push("o.CLASS COLLATE NOCASE ASC".to_string());
                parts.push("COALESCE(d.DISC, 0) ASC".to_string());
                parts.push("COALESCE(d.TRACK, 0) ASC".to_string());
                parts.push(format!("{QUERY_TITLE} COLLATE NOCASE ASC"));
            }
        }
    } else {
        for spec in sort {
            let direction = if spec.descending { "DESC" } else { "ASC" };
            parts.push(format!(
                "{} COLLATE NOCASE {direction}",
                catalog_field_sql(spec.field)
            ));
        }
    }
    // HashMap iteration previously made ties nondeterministic. Stable object
    // IDs make page boundaries repeatable across requests and restarts.
    parts.push("o.OBJECT_ID ASC".to_string());
    parts.join(", ")
}

pub struct LibraryDb {
    conn: Connection,
    pub path: PathBuf,
}

impl LibraryDb {
    /// Make long SQLite statements and lock admission observe daemon
    /// cancellation. The short busy window bounds a blocked publication;
    /// normal reconciles retry on the next watcher/periodic pass.
    pub fn install_cancellation(
        &self,
        cancellation: crate::CancellationToken,
    ) -> rusqlite::Result<()> {
        self.conn
            .busy_timeout(std::time::Duration::from_millis(250))?;
        self.conn
            .progress_handler(1_000, Some(move || cancellation.is_cancelled()));
        Ok(())
    }

    /// Capture the exact catalog rows touched by the next scanner transaction.
    /// TEMP triggers keep the journal connection-local and transactional.
    pub fn begin_catalog_change_capture(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS catalog_object_changes (
                 OBJECT_ID TEXT PRIMARY KEY
             );
             CREATE TEMP TABLE IF NOT EXISTS catalog_detail_changes (
                 DETAIL_ID INTEGER PRIMARY KEY
             );
             CREATE TEMP TABLE IF NOT EXISTS catalog_album_art_changes (
                 ALBUM_ART_ID INTEGER PRIMARY KEY
             );
             DELETE FROM catalog_object_changes;
             DELETE FROM catalog_detail_changes;
             DELETE FROM catalog_album_art_changes;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_object_insert
             AFTER INSERT ON main.OBJECTS BEGIN
                 INSERT INTO catalog_object_changes VALUES (NEW.OBJECT_ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_object_update
             AFTER UPDATE ON main.OBJECTS BEGIN
                 INSERT INTO catalog_object_changes VALUES (OLD.OBJECT_ID) ON CONFLICT DO NOTHING;
                 INSERT INTO catalog_object_changes VALUES (NEW.OBJECT_ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_object_delete
             AFTER DELETE ON main.OBJECTS BEGIN
                 INSERT INTO catalog_object_changes VALUES (OLD.OBJECT_ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_detail_insert
             AFTER INSERT ON main.DETAILS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_detail_update
             AFTER UPDATE ON main.DETAILS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
                 INSERT INTO catalog_detail_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_detail_delete
             AFTER DELETE ON main.DETAILS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_caption_insert
             AFTER INSERT ON main.CAPTIONS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_caption_delete
             AFTER DELETE ON main.CAPTIONS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_bookmark_insert
             AFTER INSERT ON main.BOOKMARKS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_bookmark_update
             AFTER UPDATE ON main.BOOKMARKS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_bookmark_delete
             AFTER DELETE ON main.BOOKMARKS BEGIN
                 INSERT INTO catalog_detail_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_album_art_insert
             AFTER INSERT ON main.ALBUM_ART BEGIN
                 INSERT INTO catalog_album_art_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_album_art_update
             AFTER UPDATE ON main.ALBUM_ART BEGIN
                 INSERT INTO catalog_album_art_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
                 INSERT INTO catalog_album_art_changes VALUES (NEW.ID) ON CONFLICT DO NOTHING;
             END;
             CREATE TEMP TRIGGER IF NOT EXISTS capture_album_art_delete
             AFTER DELETE ON main.ALBUM_ART BEGIN
                 INSERT INTO catalog_album_art_changes VALUES (OLD.ID) ON CONFLICT DO NOTHING;
             END;",
        )
    }

    pub fn quick_check(&self) -> rusqlite::Result<String> {
        self.conn
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
    }
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        Self::open_with_control(path, std::time::Duration::from_secs(15), None)
    }

    pub fn open_with_cancellation(
        path: &Path,
        cancellation: crate::CancellationToken,
    ) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        Self::open_with_control(
            path,
            std::time::Duration::from_millis(250),
            Some(cancellation),
        )
    }

    #[cfg(test)]
    fn open_with_busy_timeout(
        path: &Path,
        busy_timeout: std::time::Duration,
    ) -> rusqlite::Result<Self> {
        Self::open_with_control(path, busy_timeout, None)
    }

    fn open_with_control(
        path: &Path,
        busy_timeout: std::time::Duration,
        cancellation: Option<crate::CancellationToken>,
    ) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(busy_timeout)?;
        if let Some(cancellation) = cancellation {
            conn.progress_handler(1_000, Some(move || cancellation.is_cancelled()));
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate_schema(&mut conn)?;
        verify_integrity(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn open_memory() -> rusqlite::Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate_schema(&mut conn)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    /// Open an already-initialized catalog for bounded request-time queries.
    /// This deliberately performs no migrations or write pragmas.
    pub fn open_read_only(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn query_children_page(
        &self,
        parent_id: &str,
        sort: &[CatalogQuerySort],
        default_order: CatalogDefaultOrder,
        start: usize,
        take: usize,
    ) -> rusqlite::Result<CatalogQueryPage> {
        let from = " FROM OBJECTS o LEFT JOIN DETAILS d ON o.DETAIL_ID = d.ID ";
        let where_sql = " WHERE o.PARENT_ID = ? ";
        let params = vec![Value::Text(parent_id.to_string())];
        self.run_catalog_page(
            from,
            where_sql,
            "",
            &params,
            sort,
            default_order,
            start,
            take,
        )
    }

    pub fn query_search_page(
        &self,
        root_id: &str,
        query: &CatalogQuery,
        start: usize,
        take: usize,
    ) -> rusqlite::Result<CatalogQueryPage> {
        let all = root_id.is_empty() || root_id == ROOT_ID;
        let cte = if all {
            String::new()
        } else {
            "WITH RECURSIVE scope(OBJECT_ID) AS (\
             SELECT ? UNION SELECT child.OBJECT_ID FROM OBJECTS child \
             JOIN scope parent ON child.PARENT_ID = parent.OBJECT_ID) "
                .to_string()
        };
        let mut values = Vec::new();
        if !all {
            values.push(Value::Text(root_id.to_string()));
        }
        let mut predicates = Vec::new();
        for group in &query.groups {
            let clauses: Vec<String> = group
                .iter()
                .map(|clause| catalog_clause_sql(clause, &mut values))
                .collect();
            predicates.push(format!("({})", clauses.join(" AND ")));
        }
        let criteria = if predicates.is_empty() {
            "1".to_string()
        } else {
            format!("({})", predicates.join(" OR "))
        };
        let scope = if all {
            "1".to_string()
        } else {
            "(o.OBJECT_ID IN (SELECT OBJECT_ID FROM scope) \
              OR o.PARENT_ID IN (SELECT OBJECT_ID FROM scope) \
              OR o.REF_ID IN (SELECT OBJECT_ID FROM scope))"
                .to_string()
        };
        let where_sql = format!(
            " WHERE o.OBJECT_ID <> '{}' AND {scope} AND {criteria} ",
            ROOT_ID.replace('\'', "''")
        );
        let from = " FROM OBJECTS o LEFT JOIN DETAILS d ON o.DETAIL_ID = d.ID ";
        self.run_catalog_page(
            from,
            &where_sql,
            &cte,
            &values,
            &query.sort,
            query.default_order,
            start,
            take,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_catalog_page(
        &self,
        from: &str,
        where_sql: &str,
        cte: &str,
        values: &[Value],
        sort: &[CatalogQuerySort],
        default_order: CatalogDefaultOrder,
        start: usize,
        take: usize,
    ) -> rusqlite::Result<CatalogQueryPage> {
        let population_i64: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM OBJECTS WHERE OBJECT_ID <> ?1",
            [ROOT_ID],
            |row| row.get(0),
        )?;
        let count_sql = format!("{cte}SELECT COUNT(*){from}{where_sql}");
        let total_i64: i64 = self
            .conn
            .prepare_cached(&count_sql)?
            .query_row(params_from_iter(values.iter()), |row| row.get(0))?;
        let total = u32::try_from(total_i64.max(0)).unwrap_or(u32::MAX);
        let population = u32::try_from(population_i64.max(0)).unwrap_or(u32::MAX);
        if take == 0 || start >= total_i64.max(0) as usize {
            return Ok(CatalogQueryPage {
                object_ids: Vec::new(),
                total,
                population,
            });
        }
        let order = catalog_order_sql(sort, default_order);
        let page_sql = format!(
            "{cte}SELECT o.OBJECT_ID{from}{where_sql} ORDER BY {order} LIMIT {} OFFSET {}",
            take.min(i64::MAX as usize),
            start.min(i64::MAX as usize)
        );
        let mut stmt = self.conn.prepare_cached(&page_sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        let object_ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(CatalogQueryPage {
            object_ids,
            total,
            population,
        })
    }

    pub fn transaction(&self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn find_detail_by_inode(&self, device: i64, inode: i64) -> rusqlite::Result<Option<i64>> {
        Ok(self
            .find_inode_source(device, inode)?
            .map(|source| source.id))
    }

    pub fn remove_detail_id(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM OBJECTS WHERE DETAIL_ID = ?1", [id])?;
        self.conn
            .execute("DELETE FROM CAPTIONS WHERE ID = ?1", [id])?;
        self.conn
            .execute("DELETE FROM BOOKMARKS WHERE ID = ?1", [id])?;
        self.conn
            .execute("DELETE FROM DETAILS WHERE ID = ?1", [id])?;
        Ok(())
    }

    /// dialect `find_detail_by_inode` + TIMESTAMP so aliases can reuse
    /// metadata without re-probing when the original is still current.
    pub fn find_inode_source(
        &self,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<Option<InodeSource>> {
        if inode == 0 {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT ID, SIZE, TIMESTAMP, PATH, MIME, STREAM_PROBE_REV FROM DETAILS
                 WHERE DEVICE = ?1 AND INODE = ?2 AND MIME IS NOT NULL
                 ORDER BY STREAM_PROBE_REV DESC, ID
                 LIMIT 1",
                params![device, inode],
                |r| {
                    Ok(InodeSource {
                        id: r.get(0)?,
                        size: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        timestamp: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        path: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        mime: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        stream_probe_rev: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    })
                },
            )
            .optional()
    }

    /// dialect `clone_detail_for_path`: new DETAILS row, copied codec/date
    /// columns, new PATH/SIZE/TIMESTAMP/DEVICE/INODE.
    pub fn clone_detail_for_path(
        &self,
        src_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO DETAILS
               (PATH, SIZE, TIMESTAMP, TITLE, DURATION, BITRATE, SAMPLERATE,
                CREATOR, ARTIST, ALBUM_ARTIST, COMPOSER, CONTRIBUTOR,
                ALBUM, GENRE, COMMENT, CHANNELS, DISC, TRACK, RATING,
                DATE, RESOLUTION, THUMBNAIL, ALBUM_ART, ROTATION, DLNA_PN, MIME,
                DEVICE, INODE, CONTAINER, VIDEO, AUDIO, AUDIO_STREAMS, HDR,
                STREAM_PROBE_REV)
             SELECT ?1, ?2, ?3, TITLE, DURATION, BITRATE, SAMPLERATE,
                CREATOR, ARTIST, ALBUM_ARTIST, COMPOSER, CONTRIBUTOR,
                ALBUM, GENRE, COMMENT, CHANNELS, DISC, TRACK, RATING,
                DATE, RESOLUTION, THUMBNAIL, ALBUM_ART, ROTATION, DLNA_PN, MIME,
                ?4, ?5, CONTAINER, VIDEO, AUDIO, AUDIO_STREAMS, HDR,
                STREAM_PROBE_REV
             FROM DETAILS WHERE ID = ?6",
            params![path, size, mtime, device, inode, src_id],
        )?;
        let id = self.conn.last_insert_rowid();
        let n: i64 =
            self.conn
                .query_row("SELECT count(*) FROM CAPTIONS WHERE ID = ?1", [id], |r| {
                    r.get(0)
                })?;
        if n == 0 {
            self.conn.execute(
                "INSERT OR IGNORE INTO CAPTIONS (ID, PATH)
                 SELECT ?1, PATH FROM CAPTIONS WHERE ID = ?2",
                params![id, src_id],
            )?;
        }
        Ok(id)
    }

    pub fn all_detail_stats(&self) -> rusqlite::Result<Vec<DetailStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT PATH, ID, SIZE, TIMESTAMP, DEVICE, INODE FROM DETAILS WHERE PATH IS NOT NULL AND MIME IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DetailStat {
                path: r.get(0)?,
                id: r.get(1)?,
                size: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                timestamp: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                device: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                inode: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn detail_stats_for_paths(&self, paths: &[String]) -> rusqlite::Result<Vec<DetailStat>> {
        let mut statement = self.conn.prepare(
            "SELECT PATH, ID, SIZE, TIMESTAMP, DEVICE, INODE
             FROM DETAILS
             WHERE PATH = ?1 AND MIME IS NOT NULL",
        )?;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            if !seen.insert(path) {
                continue;
            }
            let row = statement
                .query_row([path], |row| {
                    Ok(DetailStat {
                        path: row.get(0)?,
                        id: row.get(1)?,
                        size: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        timestamp: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        device: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        inode: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    })
                })
                .optional()?;
            if let Some(row) = row {
                out.push(row);
            }
        }
        Ok(out)
    }

    pub fn video_detail_titles(&self) -> rusqlite::Result<Vec<VideoDetailTitle>> {
        let mut statement = self.conn.prepare(
            "SELECT ID, PATH, COALESCE(TITLE, ''), COALESCE(DEVICE, 0), COALESCE(INODE, 0)
             FROM DETAILS
             WHERE PATH IS NOT NULL AND MIME LIKE 'video/%'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(VideoDetailTitle {
                id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                device: row.get(3)?,
                inode: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn update_detail_title(&self, id: i64, title: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET TITLE = ?1 WHERE ID = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    pub fn find_detail_by_path(&self, path: &str) -> rusqlite::Result<Option<ExistingDetail>> {
        self.conn
            .query_row(
                "SELECT ID, SIZE, TIMESTAMP,
                        COALESCE(DEVICE, 0), COALESCE(INODE, 0)
                 FROM DETAILS WHERE PATH = ?1 LIMIT 1",
                [path],
                |r| {
                    Ok(ExistingDetail {
                        id: r.get(0)?,
                        size: r.get(1)?,
                        timestamp: r.get(2)?,
                        device: r.get(3)?,
                        inode: r.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn details_with_inode(
        &self,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<Vec<(i64, String)>> {
        if inode == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT ID, PATH FROM DETAILS
             WHERE DEVICE = ?1 AND INODE = ?2 AND MIME IS NOT NULL AND PATH IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![device, inode], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn find_inode_probe_source(
        &self,
        device: i64,
        inode: i64,
        not_id: i64,
    ) -> rusqlite::Result<Option<i64>> {
        if inode == 0 {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT ID FROM DETAILS
                 WHERE DEVICE = ?1 AND INODE = ?2 AND ID != ?3
                   AND MIME IS NOT NULL
                   AND STREAM_PROBE_REV >= 3
                   AND SIZE = (SELECT SIZE FROM DETAILS WHERE ID = ?3)
                   AND TIMESTAMP = (SELECT TIMESTAMP FROM DETAILS WHERE ID = ?3)
                 LIMIT 1",
                params![device, inode, not_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
    }

    pub fn copy_stream_from(&self, src: i64, dest: i64) -> rusqlite::Result<()> {
        if src == dest {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE DETAILS SET
                 DURATION = (SELECT DURATION FROM DETAILS WHERE ID = ?1),
                 BITRATE = (SELECT BITRATE FROM DETAILS WHERE ID = ?1),
                 RESOLUTION = (SELECT RESOLUTION FROM DETAILS WHERE ID = ?1),
                 CHANNELS = (SELECT CHANNELS FROM DETAILS WHERE ID = ?1),
                 SAMPLERATE = (SELECT SAMPLERATE FROM DETAILS WHERE ID = ?1),
                 CONTAINER = (SELECT CONTAINER FROM DETAILS WHERE ID = ?1),
                 VIDEO = (SELECT VIDEO FROM DETAILS WHERE ID = ?1),
                 AUDIO = (SELECT AUDIO FROM DETAILS WHERE ID = ?1),
                 HDR = (SELECT HDR FROM DETAILS WHERE ID = ?1),
                 AUDIO_STREAMS = (SELECT AUDIO_STREAMS FROM DETAILS WHERE ID = ?1),
                 DLNA_PN = (SELECT DLNA_PN FROM DETAILS WHERE ID = ?1),
                 STREAM_PROBE_REV = (SELECT STREAM_PROBE_REV FROM DETAILS WHERE ID = ?1)
             WHERE ID = ?2",
            params![src, dest],
        )?;
        Ok(())
    }

    pub fn upsert_album_art(&self, path: &str) -> rusqlite::Result<i64> {
        if let Some(id) = self
            .conn
            .query_row("SELECT ID FROM ALBUM_ART WHERE PATH = ?1", [path], |r| {
                r.get(0)
            })
            .optional()?
        {
            return Ok(id);
        }
        self.conn
            .execute("INSERT INTO ALBUM_ART (PATH) VALUES (?1)", [path])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_detail_album_art(&self, id: i64, art_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET ALBUM_ART = ?1 WHERE ID = ?2",
            params![art_id, id],
        )?;
        Ok(())
    }

    pub fn clear_detail_album_art(&self, id: i64) -> rusqlite::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE DETAILS SET ALBUM_ART = NULL
             WHERE ID = ?1 AND COALESCE(ALBUM_ART, 0) != 0",
            [id],
        )? > 0;
        if changed {
            self.copy_album_art_to_inode_aliases(id)?;
        }
        Ok(changed)
    }

    /// Remove database rows no longer referenced by any detail and return the
    /// backing paths. The caller may delete only paths it owns (cache files),
    /// never user-provided artwork.
    pub fn prune_unreferenced_album_art(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.PATH FROM ALBUM_ART a
             WHERE NOT EXISTS (
               SELECT 1 FROM DETAILS d WHERE COALESCE(d.ALBUM_ART, 0) = a.ID
             )",
        )?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.conn.execute(
            "DELETE FROM ALBUM_ART
             WHERE NOT EXISTS (
               SELECT 1 FROM DETAILS d WHERE COALESCE(d.ALBUM_ART, 0) = ALBUM_ART.ID
             )",
            [],
        )?;
        Ok(paths)
    }

    pub fn detail_album_art(&self, id: i64) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COALESCE(ALBUM_ART, 0) FROM DETAILS WHERE ID = ?1",
            [id],
            |r| r.get(0),
        )
    }

    pub fn album_art_path(&self, id: i64) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT PATH FROM ALBUM_ART WHERE ID = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
    }

    pub fn copy_album_art_to_inode_aliases(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET
                 ALBUM_ART = (SELECT ALBUM_ART FROM DETAILS WHERE ID = ?1)
             WHERE DEVICE = (SELECT DEVICE FROM DETAILS WHERE ID = ?1)
               AND INODE = (SELECT INODE FROM DETAILS WHERE ID = ?1)
               AND INODE != 0
               AND ID != ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn update_detail_creator_if_empty(&self, id: i64, creator: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET CREATOR = ?1
             WHERE ID = ?2 AND (CREATOR IS NULL OR CREATOR = '')",
            params![creator, id],
        )?;
        Ok(())
    }

    pub fn update_detail_nfo(&self, id: i64, nfo: &NfoMeta) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET
                 TITLE = COALESCE(?1, TITLE),
                 CREATOR = COALESCE(?2, CREATOR),
                 ARTIST = COALESCE(?3, ARTIST),
                 ALBUM = COALESCE(?4, ALBUM),
                 GENRE = COALESCE(?5, GENRE),
                 COMMENT = COALESCE(?6, COMMENT),
                 DISC = COALESCE(?7, DISC),
                 TRACK = COALESCE(?8, TRACK),
                 DATE = COALESCE(?9, DATE)
             WHERE ID = ?10",
            params![
                nfo.title,
                nfo.creator,
                nfo.artist,
                nfo.showtitle,
                nfo.genre,
                nfo.comment,
                nfo.disc,
                nfo.track,
                nfo.date,
                id
            ],
        )?;
        Ok(())
    }

    pub fn update_detail_embedded_tags(
        &self,
        id: i64,
        tags: &EmbeddedTags,
    ) -> rusqlite::Result<()> {
        let date = tags
            .date
            .as_deref()
            .map(rusty_dlna_protocol::w3c_normalize_date);
        let camera = match (
            tags.camera_make
                .as_deref()
                .filter(|value| !value.is_empty()),
            tags.camera_model
                .as_deref()
                .filter(|value| !value.is_empty()),
        ) {
            (Some(make), Some(model)) if !model.starts_with(make) => {
                Some(format!("{make} {model}"))
            }
            (Some(make), _) => Some(make.to_string()),
            (_, Some(model)) => Some(model.to_string()),
            _ => None,
        };
        self.conn.execute(
            "UPDATE DETAILS SET
                 TITLE = CASE WHEN COALESCE(MIME, '') LIKE 'video/%'
                     THEN TITLE ELSE COALESCE(NULLIF(?1, ''), TITLE) END,
                 ARTIST = COALESCE(NULLIF(?2, ''), ARTIST),
                 ALBUM_ARTIST = COALESCE(NULLIF(?3, ''), ALBUM_ARTIST),
                 ALBUM = COALESCE(NULLIF(?4, ''), ALBUM),
                 GENRE = COALESCE(NULLIF(?5, ''), GENRE),
                 COMPOSER = COALESCE(NULLIF(?6, ''), COMPOSER),
                 CONTRIBUTOR = COALESCE(NULLIF(?7, ''), CONTRIBUTOR),
                 DATE = COALESCE(NULLIF(?8, ''), DATE),
                 COMMENT = COALESCE(NULLIF(?9, ''), COMMENT),
                 DISC = COALESCE(?10, DISC),
                 TRACK = COALESCE(?11, TRACK),
                 RATING = COALESCE(?12, RATING),
                 CREATOR = COALESCE(NULLIF(?13, ''), CREATOR),
                 ROTATION = COALESCE(?14, ROTATION)
             WHERE ID = ?15",
            params![
                tags.title,
                tags.artist,
                tags.album_artist,
                tags.album,
                tags.genre,
                tags.composer,
                tags.contributor,
                date,
                tags.comment,
                tags.disc,
                tags.track,
                tags.rating,
                camera,
                tags.rotation,
                id,
            ],
        )?;
        Ok(())
    }

    pub fn reset_detail_tags_to_file_defaults(
        &self,
        id: i64,
        title: &str,
        date: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET TITLE = ?1, DATE = ?2, CREATOR = NULL,
                 ARTIST = NULL, ALBUM_ARTIST = NULL, ALBUM = NULL, GENRE = NULL,
                 COMPOSER = NULL, CONTRIBUTOR = NULL, COMMENT = NULL,
                 DISC = NULL, TRACK = NULL, RATING = NULL, ROTATION = NULL
             WHERE ID = ?3",
            params![title, date, id],
        )?;
        Ok(())
    }

    pub fn copy_embedded_tags_to_inode_aliases(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET
                 TITLE = CASE WHEN COALESCE(MIME, '') LIKE 'video/%'
                     THEN TITLE ELSE (SELECT TITLE FROM DETAILS WHERE ID = ?1) END,
                 ARTIST = (SELECT ARTIST FROM DETAILS WHERE ID = ?1),
                 ALBUM_ARTIST = (SELECT ALBUM_ARTIST FROM DETAILS WHERE ID = ?1),
                 ALBUM = (SELECT ALBUM FROM DETAILS WHERE ID = ?1),
                 GENRE = (SELECT GENRE FROM DETAILS WHERE ID = ?1),
                 COMPOSER = (SELECT COMPOSER FROM DETAILS WHERE ID = ?1),
                 CONTRIBUTOR = (SELECT CONTRIBUTOR FROM DETAILS WHERE ID = ?1),
                 DATE = (SELECT DATE FROM DETAILS WHERE ID = ?1),
                 COMMENT = (SELECT COMMENT FROM DETAILS WHERE ID = ?1),
                 DISC = (SELECT DISC FROM DETAILS WHERE ID = ?1),
                 TRACK = (SELECT TRACK FROM DETAILS WHERE ID = ?1),
                 RATING = (SELECT RATING FROM DETAILS WHERE ID = ?1),
                 CREATOR = (SELECT CREATOR FROM DETAILS WHERE ID = ?1),
                 ROTATION = (SELECT ROTATION FROM DETAILS WHERE ID = ?1)
             WHERE DEVICE = (SELECT DEVICE FROM DETAILS WHERE ID = ?1)
               AND INODE = (SELECT INODE FROM DETAILS WHERE ID = ?1)
               AND INODE != 0 AND ID != ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn copy_nfo_to_inode_aliases(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET
                 TITLE = (SELECT TITLE FROM DETAILS WHERE ID = ?1),
                 CREATOR = (SELECT CREATOR FROM DETAILS WHERE ID = ?1),
                 ARTIST = (SELECT ARTIST FROM DETAILS WHERE ID = ?1),
                 ALBUM = (SELECT ALBUM FROM DETAILS WHERE ID = ?1),
                 GENRE = (SELECT GENRE FROM DETAILS WHERE ID = ?1),
                 COMMENT = (SELECT COMMENT FROM DETAILS WHERE ID = ?1),
                 DISC = (SELECT DISC FROM DETAILS WHERE ID = ?1),
                 TRACK = (SELECT TRACK FROM DETAILS WHERE ID = ?1),
                 DATE = (SELECT DATE FROM DETAILS WHERE ID = ?1)
             WHERE DEVICE = (SELECT DEVICE FROM DETAILS WHERE ID = ?1)
               AND INODE = (SELECT INODE FROM DETAILS WHERE ID = ?1)
               AND INODE != 0
               AND ID != ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn insert_detail(&self, detail: NewDetail<'_>) -> rusqlite::Result<i64> {
        let NewDetail {
            path,
            size,
            timestamp,
            title,
            date,
            mime,
            device,
            inode,
            dlna_pn,
        } = detail;
        self.conn.execute(
            "INSERT INTO DETAILS (PATH, SIZE, TIMESTAMP, TITLE, DATE, MIME, DEVICE, INODE, DLNA_PN)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![path, size, timestamp, title, date, mime, device, inode, dlna_pn],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_detail_av_meta(
        &self,
        id: i64,
        duration: Option<&str>,
        bitrate: Option<i64>,
        resolution: Option<&str>,
        channels: Option<i64>,
        samplerate: Option<i64>,
    ) -> rusqlite::Result<()> {
        self.update_detail_stream(
            id,
            DetailStreamUpdate {
                duration,
                bitrate,
                resolution,
                channels,
                samplerate,
                ..DetailStreamUpdate::default()
            },
        )
    }

    pub fn update_detail_stream(
        &self,
        id: i64,
        stream: DetailStreamUpdate<'_>,
    ) -> rusqlite::Result<()> {
        let DetailStreamUpdate {
            duration,
            bitrate,
            resolution,
            channels,
            samplerate,
            container,
            video,
            audio,
            hdr,
        } = stream;
        self.conn.execute(
            "UPDATE DETAILS SET DURATION = COALESCE(?1, DURATION),
                 BITRATE = COALESCE(?2, BITRATE),
                 RESOLUTION = COALESCE(?3, RESOLUTION),
                 CHANNELS = COALESCE(?4, CHANNELS),
                 SAMPLERATE = COALESCE(?5, SAMPLERATE),
                 CONTAINER = COALESCE(?6, CONTAINER),
                 VIDEO = COALESCE(?7, VIDEO),
                 AUDIO = COALESCE(?8, AUDIO),
                 HDR = COALESCE(?9, HDR)
             WHERE ID = ?10",
            params![
                duration, bitrate, resolution, channels, samplerate, container, video, audio, hdr,
                id
            ],
        )?;
        Ok(())
    }

    pub fn update_detail_audio_streams(
        &self,
        id: i64,
        audio_streams: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET AUDIO_STREAMS = ?1 WHERE ID = ?2",
            params![audio_streams.filter(|value| !value.is_empty()), id],
        )?;
        Ok(())
    }

    pub fn copy_stream_to_inode_aliases(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET
                 DURATION = (SELECT DURATION FROM DETAILS WHERE ID = ?1),
                 BITRATE = (SELECT BITRATE FROM DETAILS WHERE ID = ?1),
                 RESOLUTION = (SELECT RESOLUTION FROM DETAILS WHERE ID = ?1),
                 CHANNELS = (SELECT CHANNELS FROM DETAILS WHERE ID = ?1),
                 SAMPLERATE = (SELECT SAMPLERATE FROM DETAILS WHERE ID = ?1),
                 CONTAINER = (SELECT CONTAINER FROM DETAILS WHERE ID = ?1),
                 VIDEO = (SELECT VIDEO FROM DETAILS WHERE ID = ?1),
                 AUDIO = (SELECT AUDIO FROM DETAILS WHERE ID = ?1),
                 HDR = (SELECT HDR FROM DETAILS WHERE ID = ?1),
                 AUDIO_STREAMS = (SELECT AUDIO_STREAMS FROM DETAILS WHERE ID = ?1),
                 DLNA_PN = (SELECT DLNA_PN FROM DETAILS WHERE ID = ?1),
                 STREAM_PROBE_REV = (SELECT STREAM_PROBE_REV FROM DETAILS WHERE ID = ?1)
             WHERE DEVICE = (SELECT DEVICE FROM DETAILS WHERE ID = ?1)
               AND INODE = (SELECT INODE FROM DETAILS WHERE ID = ?1)
               AND INODE != 0
               AND ID != ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn details_missing_stream_meta(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT MIN(ID), MIN(PATH) FROM DETAILS
             WHERE MIME IS NOT NULL AND PATH IS NOT NULL
               AND STREAM_PROBE_REV < 3
             GROUP BY DEVICE, INODE",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Inodes whose current revision has never been attempted. Empty optional
    /// metadata is not a retry signal: failed and unusual streams are marked
    /// attempted and retried only after their file stat changes.
    pub fn inodes_needing_stream_probe(&self) -> rusqlite::Result<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT DEVICE, INODE FROM DETAILS
             WHERE MIME IS NOT NULL AND INODE != 0
               AND STREAM_PROBE_REV < 3",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn clear_detail_stream(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET DURATION = NULL, BITRATE = NULL, RESOLUTION = NULL,
                 CHANNELS = NULL, SAMPLERATE = NULL,
                 CONTAINER = NULL, VIDEO = NULL, AUDIO = NULL, AUDIO_STREAMS = NULL, HDR = NULL,
                 DLNA_PN = NULL, STREAM_PROBE_REV = 3
             WHERE ID = ?1",
            [id],
        )?;
        self.copy_stream_to_inode_aliases(id)?;
        Ok(())
    }

    pub fn details_missing_av_meta(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        self.details_missing_stream_meta()
    }

    pub fn mark_detail_stream_probed(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET STREAM_PROBE_REV = 3 WHERE ID = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn update_detail_dlna_pn(&self, id: i64, pn: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET DLNA_PN = ?1 WHERE ID = ?2",
            params![pn, id],
        )?;
        Ok(())
    }

    /// Derive DLNA_PN and rewrite leftover `VIDEO=other` AVI rows from
    /// already-stored stream columns. No libav.
    pub fn backfill_derived_stream_fields(&self) -> rusqlite::Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT ID, PATH, MIME, CONTAINER, VIDEO, AUDIO, AUDIO_STREAMS, HDR, RESOLUTION, DLNA_PN
             FROM DETAILS WHERE MIME IS NOT NULL AND PATH IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut pending = Vec::new();
        for row in rows {
            pending.push(row?);
        }
        drop(stmt);
        let mut n = 0usize;
        for (id, path, mime, container, video, audio, audio_streams, hdr, resolution, pn) in pending
        {
            let ext = mime_to_ext(&mime);
            let decoded_path = crate::path_from_db(&path);
            let ext = if ext == "dat" {
                decoded_path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or(ext)
            } else {
                ext
            };
            let probe = crate::probe_from_stored(
                ext,
                container.as_deref(),
                video.as_deref(),
                audio.as_deref(),
                audio_streams.as_deref(),
                hdr.as_deref(),
                resolution.as_deref(),
            );
            if probe.video != video.as_deref().unwrap_or("") && !probe.video.is_empty() {
                self.conn.execute(
                    "UPDATE DETAILS SET VIDEO = ?1 WHERE ID = ?2",
                    params![probe.video.as_str(), id],
                )?;
                n += 1;
            }
            if probe.hdr.is_empty() && probe.video.is_empty() {
                continue;
            }
            let want = crate::dlna_pn_from_probe(
                &probe.container,
                &probe.video,
                &probe.audio,
                &probe.hdr,
                probe.width,
                probe.height,
            );
            let have = pn.filter(|s| !s.is_empty());
            if have.as_deref() != want.as_deref() {
                self.update_detail_dlna_pn(id, want.as_deref())?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn update_detail_stat(
        &self,
        id: i64,
        size: i64,
        timestamp: i64,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET SIZE = ?1, TIMESTAMP = ?2, DEVICE = ?3, INODE = ?4,
                 STREAM_PROBE_REV = 0
             WHERE ID = ?5",
            params![size, timestamp, device, inode, id],
        )?;
        Ok(())
    }

    pub fn upsert_object(
        &self,
        object_id: &str,
        parent_id: &str,
        class: &str,
        detail_id: Option<i64>,
        name: &str,
        ref_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, REF_ID, CLASS, DETAIL_ID, NAME)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(OBJECT_ID) DO UPDATE SET
               PARENT_ID = excluded.PARENT_ID,
               REF_ID = excluded.REF_ID,
               CLASS = excluded.CLASS,
               DETAIL_ID = excluded.DETAIL_ID,
               NAME = excluded.NAME",
            params![object_id, parent_id, ref_id, class, detail_id, name],
        )?;
        Ok(())
    }

    pub fn replace_captions(&self, detail_id: i64, caps: &[Caption]) -> rusqlite::Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT PATH FROM CAPTIONS WHERE ID = ?1 ORDER BY PATH")?;
        let current = stmt
            .query_map([detail_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut desired: Vec<String> = caps
            .iter()
            .map(|caption| crate::path_to_db(&caption.path))
            .collect();
        desired.sort();
        if current == desired {
            return Ok(false);
        }
        drop(stmt);
        self.conn
            .execute("DELETE FROM CAPTIONS WHERE ID = ?1", [detail_id])?;
        for c in caps {
            self.conn.execute(
                "INSERT OR IGNORE INTO CAPTIONS (ID, PATH) VALUES (?1, ?2)",
                params![detail_id, crate::path_to_db(&c.path)],
            )?;
        }
        Ok(true)
    }

    pub fn detail_presentation(&self, id: i64) -> rusqlite::Result<DetailPresentation> {
        let mut presentation = self.conn.query_row(
            "SELECT TITLE, CREATOR, ARTIST, ALBUM_ARTIST, ALBUM, GENRE,
                    COMPOSER, CONTRIBUTOR, COMMENT, DISC, TRACK, DATE, RATING,
                    ROTATION, COALESCE(ALBUM_ART, 0)
             FROM DETAILS WHERE ID = ?1",
            [id],
            |row| {
                Ok(DetailPresentation {
                    title: row.get(0)?,
                    creator: row.get(1)?,
                    artist: row.get(2)?,
                    album_artist: row.get(3)?,
                    album: row.get(4)?,
                    genre: row.get(5)?,
                    composer: row.get(6)?,
                    contributor: row.get(7)?,
                    comment: row.get(8)?,
                    disc: row.get(9)?,
                    track: row.get(10)?,
                    date: row.get(11)?,
                    rating: row.get(12)?,
                    rotation: row.get(13)?,
                    album_art: row.get(14)?,
                    captions: Vec::new(),
                })
            },
        )?;
        let mut stmt = self
            .conn
            .prepare("SELECT PATH FROM CAPTIONS WHERE ID = ?1 ORDER BY PATH")?;
        presentation.captions = stmt
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(presentation)
    }

    pub fn all_video_has_inode(&self, device: i64, inode: i64) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM OBJECTS o JOIN DETAILS d ON o.DETAIL_ID = d.ID
             WHERE o.PARENT_ID = ?1 AND d.DEVICE = ?2 AND d.INODE = ?3",
            params![VIDEO_ALL_ID, device, inode],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Next `$HEX` suffix for a new child of `parent`. Uses the max existing
    /// suffix, not `count(*)+1`, so deleting a sibling cannot reuse an id
    /// and `upsert` cannot rename another folder onto leftover children.
    pub fn next_child_seq(&self, parent: &str) -> rusqlite::Result<i64> {
        let prefix = format!("{parent}$");
        let mut stmt = self
            .conn
            .prepare("SELECT OBJECT_ID FROM OBJECTS WHERE PARENT_ID = ?1")?;
        let rows = stmt.query_map([parent], |r| r.get::<_, String>(0))?;
        let mut max = 0i64;
        for id in rows {
            let id = id?;
            let Some(rest) = id.strip_prefix(&prefix) else {
                continue;
            };
            let suffix = rest.split('$').next().unwrap_or(rest);
            if let Ok(n) = i64::from_str_radix(suffix, 16) {
                max = max.max(n);
            }
        }
        Ok(max + 1)
    }

    pub fn object_exists(&self, object_id: &str) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
    }

    /// All OBJECTS rows except virtual roots. Used so `rebuild_objects`
    /// can put the same IDs back — Infuse/libupnp caches ObjectID.
    pub fn snapshot_objects(&self) -> rusqlite::Result<Vec<ObjectSnap>> {
        let mut stmt = self
            .conn
            .prepare("SELECT OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID, NAME, REF_ID FROM OBJECTS")?;
        let rows = stmt.query_map([], |r| {
            Ok(ObjectSnap {
                object_id: r.get(0)?,
                parent_id: r.get(1)?,
                class: r.get(2)?,
                detail_id: r.get(3)?,
                name: r.get(4)?,
                ref_id: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if is_virtual_container(&row.object_id) {
                continue;
            }
            out.push(row);
        }
        Ok(out)
    }

    pub fn restore_object(&self, row: &ObjectSnap) -> rusqlite::Result<()> {
        self.upsert_object(
            &row.object_id,
            &row.parent_id,
            &row.class,
            row.detail_id,
            row.name.as_deref().unwrap_or(""),
            row.ref_id.as_deref(),
        )
    }

    /// Fields used to hang Series/Genre aliases and deduplicate physical files.
    pub fn detail_group_fields(&self, id: i64) -> rusqlite::Result<DetailGroupFields> {
        self.conn.query_row(
            "SELECT ALBUM, GENRE, DISC, TRACK, TITLE,
                    COALESCE(DEVICE, 0), COALESCE(INODE, 0)
             FROM DETAILS WHERE ID = ?1",
            [id],
            |r| {
                Ok(DetailGroupFields {
                    album: r.get(0)?,
                    genre: r.get(1)?,
                    disc: r.get(2)?,
                    track: r.get(3)?,
                    title: r.get(4)?,
                    device: r.get(5)?,
                    inode: r.get(6)?,
                })
            },
        )
    }

    pub fn detail_tag_fields(&self, id: i64) -> rusqlite::Result<DetailTags> {
        self.conn.query_row(
            "SELECT TITLE, ARTIST, ALBUM_ARTIST, ALBUM, GENRE, COMPOSER,
                    CONTRIBUTOR, CREATOR, DATE, RATING
             FROM DETAILS WHERE ID = ?1",
            [id],
            |r| {
                Ok(DetailTags {
                    title: r.get(0)?,
                    artist: r.get(1)?,
                    album_artist: r.get(2)?,
                    album: r.get(3)?,
                    genre: r.get(4)?,
                    composer: r.get(5)?,
                    contributor: r.get(6)?,
                    creator: r.get(7)?,
                    date: r.get(8)?,
                    rating: r.get(9)?,
                })
            },
        )
    }

    /// Browse Folders object for this detail (no `REF_ID`).
    pub fn browse_object_for_detail(&self, detail: i64) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT OBJECT_ID FROM OBJECTS
                 WHERE DETAIL_ID = ?1 AND REF_ID IS NULL
                 ORDER BY length(OBJECT_ID) ASC LIMIT 1",
                [detail],
                |r| r.get(0),
            )
            .optional()
    }

    /// Drop Series/Genre item aliases for this detail so a re-NFO can move them.
    pub fn delete_detail_under_root(&self, detail: i64, root: &str) -> rusqlite::Result<usize> {
        let like = format!("{root}$%");
        self.conn.execute(
            "DELETE FROM OBJECTS WHERE DETAIL_ID = ?1 AND (
                 PARENT_ID = ?2 OR PARENT_ID LIKE ?3 OR OBJECT_ID LIKE ?3
             )",
            params![detail, root, like],
        )
    }

    /// Refresh the presentation name of an item's aliases under a virtual
    /// root without disturbing its stable ObjectIDs.
    pub fn update_detail_names_under_root(
        &self,
        detail: i64,
        root: &str,
        name: &str,
    ) -> rusqlite::Result<usize> {
        let like = format!("{root}$%");
        self.conn.execute(
            "UPDATE OBJECTS SET NAME = ?1
             WHERE DETAIL_ID = ?2 AND (PARENT_ID = ?3 OR PARENT_ID LIKE ?4)
               AND COALESCE(NAME, '') <> ?1",
            params![name, detail, root, like],
        )
    }

    pub fn object_detail_id(&self, object_id: &str) -> rusqlite::Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT DETAIL_ID FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Delete this path. Also drop other DETAILS rows for the same inode
    /// whose files are gone (dangling symlink aliases). Live hardlinks and
    /// live symlinks that still resolve — e.g. a genre tree retargeted at
    /// the file's new location — are kept.
    pub fn remove_path_and_symlink_aliases(&self, path: &str) -> rusqlite::Result<usize> {
        let row = self
            .conn
            .query_row(
                "SELECT ID, DEVICE, INODE FROM DETAILS WHERE PATH = ?1 LIMIT 1",
                [path],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((_, device, inode)) = row else {
            return Ok(0);
        };
        let mut victims: Vec<(i64, String)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT ID, PATH FROM DETAILS WHERE DEVICE = ?1 AND INODE = ?2")?;
            let rows = stmt.query_map(params![device, inode], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for r in rows {
                victims.push(r?);
            }
        }
        let mut n = 0usize;
        for (id, p) in victims {
            let gone = p == path || !path_is_live_file(&path_from_db(&p));
            if !gone {
                continue;
            }
            self.conn
                .execute("DELETE FROM OBJECTS WHERE DETAIL_ID = ?1", [id])?;
            self.conn
                .execute("DELETE FROM CAPTIONS WHERE ID = ?1", [id])?;
            self.conn
                .execute("DELETE FROM BOOKMARKS WHERE ID = ?1", [id])?;
            self.conn
                .execute("DELETE FROM DETAILS WHERE ID = ?1", [id])?;
            n += 1;
        }
        Ok(n)
    }

    pub fn prune_missing_files(&self) -> rusqlite::Result<usize> {
        let mut paths: Vec<String> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT PATH FROM DETAILS WHERE PATH IS NOT NULL AND MIME IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for r in rows {
                paths.push(r?);
            }
        }
        let mut n = 0;
        for p in paths {
            if !path_is_live_file(&path_from_db(&p)) {
                n += self.remove_path_and_symlink_aliases(&p)?;
            }
        }
        Ok(n)
    }

    pub fn prune_excluded_paths(&self, cfg: &ScanConfig) -> rusqlite::Result<usize> {
        let mut paths: Vec<String> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT PATH FROM DETAILS WHERE PATH IS NOT NULL AND MIME IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for r in rows {
                paths.push(r?);
            }
        }
        let mut n = 0;
        for p in paths {
            if path_is_unwanted(&path_from_db(&p), cfg) {
                n += self.remove_path_and_symlink_aliases(&p)?;
            }
        }
        Ok(n)
    }

    /// Drop folder OBJECTS that have no remaining children. Virtual
    /// containers (root, Browse Folders, All Video, …) stay even if empty.
    pub fn prune_empty_folders(&self) -> rusqlite::Result<usize> {
        let mut n = 0usize;
        loop {
            let empty: Vec<String> = {
                let mut stmt = self.conn.prepare(
                    "SELECT o.OBJECT_ID FROM OBJECTS o
                     WHERE o.DETAIL_ID IS NULL
                       AND NOT EXISTS (
                         SELECT 1 FROM OBJECTS c WHERE c.PARENT_ID = o.OBJECT_ID
                       )",
                )?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                let mut ids = Vec::new();
                for r in rows {
                    let id = r?;
                    if !is_virtual_container(&id) {
                        ids.push(id);
                    }
                }
                ids
            };
            if empty.is_empty() {
                break;
            }
            for id in empty {
                n += self
                    .conn
                    .execute("DELETE FROM OBJECTS WHERE OBJECT_ID = ?1", [id])?;
            }
        }
        Ok(n)
    }

    pub fn object_item_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT count(*) FROM OBJECTS WHERE DETAIL_ID IS NOT NULL",
            [],
            |r| r.get(0),
        )
    }

    pub fn details_missing_objects(&self) -> rusqlite::Result<i64> {
        // A hardlink/symlink clone may leave an older DETAILS row without
        // OBJECTS while the same inode is already attached. Those are not
        // missing library entries — do not trigger a full tree repair.
        self.conn.query_row(
            "SELECT count(*) FROM DETAILS d
             WHERE d.MIME IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM OBJECTS o WHERE o.DETAIL_ID = d.ID)
               AND NOT EXISTS (
                 SELECT 1 FROM DETAILS d2
                 JOIN OBJECTS o2 ON o2.DETAIL_ID = d2.ID
                 WHERE d2.DEVICE = d.DEVICE AND d2.INODE = d.INODE
               )",
            [],
            |r| r.get(0),
        )
    }

    pub fn detail_has_object(&self, detail_id: i64) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT count(*) FROM OBJECTS WHERE DETAIL_ID = ?1",
                [detail_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
    }

    pub fn detail_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT count(*) FROM DETAILS WHERE MIME IS NOT NULL",
            [],
            |r| r.get(0),
        )
    }

    pub fn get_update_id(&self) -> rusqlite::Result<u32> {
        let value = self
            .conn
            .query_row(
                "SELECT VALUE FROM SETTINGS WHERE KEY = 'updateID' LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        match value {
            None => Ok(1),
            Some(value) => value.parse::<u32>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            }),
        }
    }

    pub fn set_update_id(&self, id: u32) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO SETTINGS (KEY, VALUE) VALUES ('updateID', ?1)
             ON CONFLICT(KEY) DO UPDATE SET VALUE = excluded.VALUE",
            [id.to_string()],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT VALUE FROM SETTINGS WHERE KEY = ?1 ORDER BY rowid DESC LIMIT 1",
                [key],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO SETTINGS (KEY, VALUE) VALUES (?1, ?2)
             ON CONFLICT(KEY) DO UPDATE SET VALUE = excluded.VALUE",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn playlists(&self) -> rusqlite::Result<Vec<PlaylistDbRow>> {
        let mut statement = self.conn.prepare(
            "SELECT ID, NAME, PATH, COALESCE(TIMESTAMP, 0),
                    COALESCE(DEVICE, 0), COALESCE(INODE, 0)
             FROM PLAYLISTS ORDER BY ID",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(PlaylistDbRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    timestamp: row.get(3)?,
                    device: row.get(4)?,
                    inode: row.get(5)?,
                })
            })?
            .collect();
        rows
    }

    pub fn upsert_playlist(
        &self,
        existing_id: Option<i64>,
        name: &str,
        path: &str,
        timestamp: i64,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<i64> {
        if let Some(id) = existing_id {
            self.conn.execute(
                "UPDATE PLAYLISTS SET NAME=?1, PATH=?2, TIMESTAMP=?3,
                     DEVICE=?4, INODE=?5, FOUND=1 WHERE ID=?6",
                params![name, path, timestamp, device, inode, id],
            )?;
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO PLAYLISTS (NAME, PATH, TIMESTAMP, DEVICE, INODE, FOUND)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![name, path, timestamp, device, inode],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn playlist_detail_ids(&self, playlist_id: i64) -> rusqlite::Result<Vec<i64>> {
        let mut statement = self.conn.prepare(
            "SELECT DETAIL_ID FROM PLAYLIST_ITEMS
             WHERE PLAYLIST_ID=?1 ORDER BY POSITION",
        )?;
        let rows = statement
            .query_map([playlist_id], |row| row.get(0))?
            .collect();
        rows
    }

    pub fn replace_playlist_items(
        &self,
        playlist_id: i64,
        detail_ids: &[i64],
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM PLAYLIST_ITEMS WHERE PLAYLIST_ID=?1",
            [playlist_id],
        )?;
        let mut statement = self.conn.prepare(
            "INSERT INTO PLAYLIST_ITEMS (PLAYLIST_ID, DETAIL_ID, POSITION)
             VALUES (?1, ?2, ?3)",
        )?;
        for (position, detail_id) in detail_ids.iter().enumerate() {
            statement.execute(params![playlist_id, detail_id, position as i64])?;
        }
        self.conn.execute(
            "UPDATE PLAYLISTS SET ITEMS=?1 WHERE ID=?2",
            params![detail_ids.len() as i64, playlist_id],
        )?;
        Ok(())
    }

    pub fn reset_playlist_found(&self) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE PLAYLISTS SET FOUND=0", [])?;
        Ok(())
    }

    pub fn delete_missing_playlists(&self) -> rusqlite::Result<usize> {
        self.conn.execute("DELETE FROM PLAYLISTS WHERE FOUND=0", [])
    }

    pub fn clear_playlist_objects(&self) -> rusqlite::Result<()> {
        for root in [MUSIC_PLIST_ID, VIDEO_PLIST_ID, IMAGE_PLIST_ID] {
            self.conn.execute(
                "DELETE FROM OBJECTS WHERE OBJECT_ID LIKE ?1 ESCAPE '\\'",
                [format!(
                    "{}$%",
                    root.replace('%', "\\%").replace('_', "\\_")
                )],
            )?;
        }
        Ok(())
    }

    pub fn playlist_object_source(
        &self,
        detail_id: i64,
    ) -> rusqlite::Result<Option<(String, String, String)>> {
        self.conn
            .query_row(
                "SELECT OBJECT_ID, CLASS, COALESCE(NULLIF(d.TITLE, ''), NULLIF(o.NAME, ''), 'item')
                 FROM OBJECTS o JOIN DETAILS d ON d.ID=o.DETAIL_ID
                 WHERE o.DETAIL_ID=?1 AND o.REF_ID IS NULL
                 ORDER BY length(o.OBJECT_ID), o.OBJECT_ID LIMIT 1",
                [detail_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
    }

    pub fn bump_update_id(&self) -> rusqlite::Result<u32> {
        let n = self.get_update_id()?.saturating_add(1);
        self.set_update_id(n)?;
        Ok(n)
    }

    /// Atomically update one or both Kodi state fields and refresh their
    /// retention timestamp. `None` leaves that field unchanged.
    pub fn update_bookmark(
        &self,
        detail_id: i64,
        sec: Option<i64>,
        watch_count: Option<i64>,
    ) -> rusqlite::Result<()> {
        if sec.is_none() && watch_count.is_none() {
            return Ok(());
        }
        let updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO BOOKMARKS (ID, SEC, WATCH_COUNT, UPDATED_AT)
               VALUES (?1, COALESCE(?2, 0), COALESCE(?3, 0), ?4)
             ON CONFLICT(ID) DO UPDATE SET
               SEC = COALESCE(?2, BOOKMARKS.SEC),
               WATCH_COUNT = COALESCE(?3, BOOKMARKS.WATCH_COUNT),
               UPDATED_AT = ?4",
            params![detail_id, sec, watch_count, updated_at],
        )?;
        Ok(())
    }

    /// `BOOKMARKS` is keyed by DETAILS.ID. `sec < 30` is stored as 0 by the SOAP helper.
    pub fn set_bookmark(&self, detail_id: i64, sec: i64) -> rusqlite::Result<()> {
        self.update_bookmark(detail_id, Some(sec), None)
    }

    pub fn set_watch_count(&self, detail_id: i64, count: i64) -> rusqlite::Result<()> {
        self.update_bookmark(detail_id, None, Some(count))
    }

    pub fn get_bookmark(&self, detail_id: i64) -> rusqlite::Result<Option<(i64, i64)>> {
        self.conn
            .query_row(
                "SELECT COALESCE(SEC, 0), COALESCE(WATCH_COUNT, 0) FROM BOOKMARKS WHERE ID = ?1",
                [detail_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Remove resume/play-count rows older than the configured number of
    /// 24-hour days. Zero preserves bookmarks indefinitely.
    pub fn prune_expired_bookmarks(
        &self,
        retention_days: u32,
        now_unix: i64,
    ) -> rusqlite::Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let retention_secs = i64::from(retention_days).saturating_mul(86_400);
        let cutoff = now_unix.saturating_sub(retention_secs);
        self.conn.execute(
            "DELETE FROM BOOKMARKS WHERE UPDATED_AT > 0 AND UPDATED_AT < ?1",
            [cutoff],
        )
    }

    pub fn clear_objects(&self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM OBJECTS", [])?;
        Ok(())
    }

    pub fn seed_virtual_containers(&self) -> rusqlite::Result<()> {
        let rows = [
            (ROOT_ID, "-1", "container.storageFolder", "root"),
            (
                BROWSEDIR_ID,
                ROOT_ID,
                "container.storageFolder",
                "Browse Folders",
            ),
            (MUSIC_ID, ROOT_ID, "container.storageFolder", "Music"),
            (VIDEO_ID, ROOT_ID, "container.storageFolder", "Video"),
            (IMAGE_ID, ROOT_ID, "container.storageFolder", "Pictures"),
            (
                VIDEO_ALL_ID,
                VIDEO_ID,
                "container.storageFolder",
                "All Video",
            ),
            (VIDEO_DIR_ID, VIDEO_ID, "container.storageFolder", "Folders"),
            (
                VIDEO_RECENT_ID,
                VIDEO_ID,
                "container.storageFolder",
                "Recently Added",
            ),
            (
                VIDEO_SERIES_ID,
                VIDEO_ID,
                "container.storageFolder",
                "Series",
            ),
            (VIDEO_GENRE_ID, VIDEO_ID, "container.storageFolder", "Genre"),
            (VIDEO_ACTOR_ID, VIDEO_ID, "container.storageFolder", "Actor"),
            (
                VIDEO_PLIST_ID,
                VIDEO_ID,
                "container.storageFolder",
                "Playlists",
            ),
            (
                VIDEO_RATING_ID,
                VIDEO_ID,
                "container.storageFolder",
                "Rating",
            ),
            (
                MUSIC_ALL_ID,
                MUSIC_ID,
                "container.storageFolder",
                "All Music",
            ),
            (MUSIC_GENRE_ID, MUSIC_ID, "container.storageFolder", "Genre"),
            (
                MUSIC_ARTIST_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Artist",
            ),
            (MUSIC_ALBUM_ID, MUSIC_ID, "container.storageFolder", "Album"),
            (MUSIC_DIR_ID, MUSIC_ID, "container.storageFolder", "Folders"),
            (
                MUSIC_PLIST_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Playlists",
            ),
            (
                MUSIC_CONTRIB_ARTIST_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Contributing Artists",
            ),
            (
                MUSIC_ALBUM_ARTIST_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Album Artist",
            ),
            (
                MUSIC_COMPOSER_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Composer",
            ),
            (
                MUSIC_RATING_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Rating",
            ),
            (
                MUSIC_RECENT_ID,
                MUSIC_ID,
                "container.storageFolder",
                "Recently Added",
            ),
            (
                IMAGE_ALL_ID,
                IMAGE_ID,
                "container.storageFolder",
                "All Pictures",
            ),
            (
                IMAGE_DATE_ID,
                IMAGE_ID,
                "container.storageFolder",
                "Date Taken",
            ),
            (IMAGE_ALBUM_ID, IMAGE_ID, "container.storageFolder", "Album"),
            (
                IMAGE_CAMERA_ID,
                IMAGE_ID,
                "container.storageFolder",
                "Camera",
            ),
            (IMAGE_DIR_ID, IMAGE_ID, "container.storageFolder", "Folders"),
            (
                IMAGE_PLIST_ID,
                IMAGE_ID,
                "container.storageFolder",
                "Playlists",
            ),
            (
                IMAGE_RATING_ID,
                IMAGE_ID,
                "container.storageFolder",
                "Rating",
            ),
            (
                IMAGE_RECENT_ID,
                IMAGE_ID,
                "container.storageFolder",
                "Recently Added",
            ),
        ];
        for (id, parent, class, name) in rows {
            self.upsert_object(id, parent, class, None, name, None)?;
        }
        Ok(())
    }

    /// True when this folder already lists `name` for this inode (a
    /// directory-symlink alias of the same file).
    pub fn folder_has_inode_named(
        &self,
        parent_id: &str,
        device: i64,
        inode: i64,
        name: &str,
    ) -> rusqlite::Result<bool> {
        if inode == 0 {
            return Ok(false);
        }
        self.conn
            .query_row(
                "SELECT count(*) FROM OBJECTS o
                 JOIN DETAILS d ON d.ID = o.DETAIL_ID
                 WHERE o.PARENT_ID = ?1 AND d.DEVICE = ?2 AND d.INODE = ?3
                   AND o.NAME = ?4",
                params![parent_id, device, inode, name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
    }

    /// True when a virtual folder already contains this physical file. Its
    /// symlink aliases may have different DETAILS IDs, but should only occur
    /// once in Series/Genre/Actor views.
    pub fn folder_has_inode(
        &self,
        parent_id: &str,
        device: i64,
        inode: i64,
    ) -> rusqlite::Result<bool> {
        if inode == 0 {
            return Ok(false);
        }
        self.conn
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM OBJECTS o
                   JOIN DETAILS d ON d.ID = o.DETAIL_ID
                   WHERE o.PARENT_ID = ?1 AND d.DEVICE = ?2 AND d.INODE = ?3
                 )",
                params![parent_id, device, inode],
                |r| r.get::<_, i64>(0),
            )
            .map(|value| value != 0)
    }

    /// Same inode+title listed more than once in one folder.
    pub fn folders_have_duplicate_inodes(&self) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM OBJECTS o
                   JOIN DETAILS d ON d.ID = o.DETAIL_ID
                   WHERE o.DETAIL_ID IS NOT NULL AND d.INODE != 0
                   GROUP BY o.PARENT_ID, d.DEVICE, d.INODE, o.NAME
                   HAVING COUNT(*) > 1
                 )",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|value| value != 0)
    }

    /// Keep the oldest stable ObjectID when an older scanner attached several
    /// DETAILS aliases for one inode under the same virtual folder.
    pub fn prune_duplicate_folder_inodes(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM OBJECTS WHERE ID IN (
               SELECT item.ID
               FROM OBJECTS item
               JOIN DETAILS detail ON detail.ID = item.DETAIL_ID
               JOIN (
                 SELECT o.PARENT_ID AS parent_id, d.DEVICE AS device,
                        d.INODE AS inode, o.NAME AS name, MIN(o.ID) AS keep_id
                 FROM OBJECTS o
                 JOIN DETAILS d ON d.ID = o.DETAIL_ID
                 WHERE d.INODE != 0
                 GROUP BY o.PARENT_ID, d.DEVICE, d.INODE, o.NAME
                 HAVING COUNT(*) > 1
               ) duplicate
                 ON duplicate.parent_id = item.PARENT_ID
                AND duplicate.device = detail.DEVICE
                AND duplicate.inode = detail.INODE
                AND duplicate.name IS item.NAME
               WHERE item.ID != duplicate.keep_id
             )",
            [],
        )
    }

    pub fn find_child_object(
        &self,
        parent_id: &str,
        name: &str,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT OBJECT_ID FROM OBJECTS WHERE PARENT_ID = ?1 AND NAME = ?2 LIMIT 1",
                params![parent_id, name],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn object_name(&self, object_id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT NAME FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn load_catalog_patch(&self) -> rusqlite::Result<CatalogPatch> {
        let mut patch = CatalogPatch::default();
        {
            let mut statement = self
                .conn
                .prepare("SELECT OBJECT_ID FROM temp.catalog_object_changes ORDER BY OBJECT_ID")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                patch.changed_object_ids.push(row?);
            }
        }
        {
            let mut statement = self
                .conn
                .prepare("SELECT DETAIL_ID FROM temp.catalog_detail_changes ORDER BY DETAIL_ID")?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            for row in rows {
                patch.changed_detail_ids.push(row?);
            }
        }
        {
            let mut statement = self.conn.prepare(
                "SELECT ALBUM_ART_ID FROM temp.catalog_album_art_changes ORDER BY ALBUM_ART_ID",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            for row in rows {
                patch.changed_album_art_ids.push(row?);
            }
        }

        let affected_details = "SELECT DETAIL_ID FROM temp.catalog_detail_changes
             UNION
             SELECT DETAIL_ID FROM OBJECTS
             WHERE OBJECT_ID IN (SELECT OBJECT_ID FROM temp.catalog_object_changes)
               AND DETAIL_ID IS NOT NULL";
        let mut captions: HashMap<i64, Vec<Caption>> = HashMap::new();
        {
            let sql = format!(
                "SELECT ID, PATH FROM CAPTIONS WHERE ID IN ({affected_details}) ORDER BY PATH"
            );
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (detail_id, stored) = row?;
                let path = path_from_db(&stored);
                let ext = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(|value| crate::caption_ext(&format!("x.{value}")))
                    .unwrap_or("sub")
                    .to_string();
                let entries = captions.entry(detail_id).or_default();
                entries.push(Caption {
                    index: entries.len() as u32,
                    path,
                    ext,
                });
            }
        }
        {
            let mut statement = self.conn.prepare(
                "SELECT OBJECT_ID, PARENT_ID, CLASS, NAME
                 FROM OBJECTS
                 WHERE DETAIL_ID IS NULL
                   AND OBJECT_ID IN (SELECT OBJECT_ID FROM temp.catalog_object_changes)
                 ORDER BY OBJECT_ID",
            )?;
            let rows = statement.query_map([], |row| {
                let object_id: String = row.get(0)?;
                Ok(Container {
                    parent_id: row.get(1)?,
                    class: row.get(2)?,
                    title: row
                        .get::<_, Option<String>>(3)?
                        .unwrap_or_else(|| object_id.clone()),
                    object_id,
                    children: Vec::new(),
                    searchable: true,
                })
            })?;
            for row in rows {
                patch.containers.push(row?);
            }
        }
        {
            let sql = format!(
                "{CATALOG_ITEM_SELECT}
                 WHERE o.OBJECT_ID IN (SELECT OBJECT_ID FROM temp.catalog_object_changes)
                    OR o.DETAIL_ID IN (SELECT DETAIL_ID FROM temp.catalog_detail_changes)
                 ORDER BY o.OBJECT_ID"
            );
            let mut statement = self.conn.prepare(&sql)?;
            let rows =
                statement.query_map([], |row| media_item_from_catalog_row(row, &captions))?;
            for row in rows {
                patch.items.push(row?);
            }
        }
        let mut bookmarks = HashMap::new();
        {
            let sql = format!(
                "SELECT ID, COALESCE(SEC, 0), COALESCE(WATCH_COUNT, 0)
                 FROM BOOKMARKS WHERE ID IN ({affected_details})"
            );
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (detail_id, seconds, watch_count) = row?;
                bookmarks.insert(detail_id, (seconds, watch_count));
            }
        }
        for item in &mut patch.items {
            if let Some(&(seconds, watch_count)) = bookmarks.get(&item.detail_id) {
                item.bookmark_sec = seconds;
                item.watch_count = watch_count;
            }
        }
        {
            let mut statement = self.conn.prepare(
                "SELECT ID, PATH FROM ALBUM_ART
                 WHERE ID IN (SELECT ALBUM_ART_ID FROM temp.catalog_album_art_changes)",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (album_art_id, stored) = row?;
                patch
                    .album_art_paths
                    .insert(album_art_id, path_from_db(&stored));
            }
        }
        Ok(patch)
    }

    pub fn load_catalog(&self) -> rusqlite::Result<Catalog> {
        let mut cat = Catalog::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT OBJECT_ID, PARENT_ID, CLASS, NAME FROM OBJECTS WHERE DETAIL_ID IS NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (id, parent, class, name) = row?;
                if id == ROOT_ID {
                    continue;
                }
                cat.containers
                    .entry(id.clone())
                    .or_insert_with(|| Container {
                        object_id: id.clone(),
                        parent_id: parent.clone(),
                        title: name.unwrap_or_else(|| id.clone()),
                        class,
                        children: Vec::new(),
                        searchable: true,
                    });
                if let Some(p) = cat.containers.get_mut(&parent) {
                    if !p.children.iter().any(|c| c == &id) {
                        p.children.push(id);
                    }
                }
            }
        }
        let mut caps_by: HashMap<i64, Vec<Caption>> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT ID, PATH FROM CAPTIONS ORDER BY PATH")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, p) = row?;
                let decoded = crate::path_from_db(&p);
                let ext = decoded
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(|value| crate::caption_ext(&format!("x.{value}")))
                    .unwrap_or("sub")
                    .to_string();
                let e = caps_by.entry(id).or_default();
                let index = e.len() as u32;
                e.push(Caption {
                    index,
                    path: decoded,
                    ext,
                });
            }
        }
        {
            let mut stmt = self.conn.prepare(
                "SELECT o.OBJECT_ID, o.PARENT_ID, o.CLASS, o.NAME, o.DETAIL_ID, o.REF_ID,
                        d.PATH, d.SIZE, d.TIMESTAMP, d.DATE, d.MIME, d.DLNA_PN,
                        d.DEVICE, d.INODE, d.DURATION, d.BITRATE, d.RESOLUTION,
                        d.CHANNELS, d.SAMPLERATE, d.CONTAINER, d.VIDEO, d.AUDIO,
                        d.AUDIO_STREAMS, d.HDR,
                        d.ALBUM_ART, d.TITLE, d.CREATOR, d.ARTIST, d.ALBUM, d.GENRE,
                        d.COMMENT, d.DISC, d.TRACK, d.ALBUM_ARTIST, d.COMPOSER,
                        d.CONTRIBUTOR, d.RATING, d.ROTATION
                 FROM OBJECTS o JOIN DETAILS d ON o.DETAIL_ID = d.ID
                 WHERE o.DETAIL_ID IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, Option<i64>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, Option<String>>(16)?,
                    r.get::<_, Option<i64>>(17)?,
                    r.get::<_, Option<i64>>(18)?,
                    r.get::<_, Option<String>>(19)?,
                    r.get::<_, Option<String>>(20)?,
                    r.get::<_, Option<String>>(21)?,
                    r.get::<_, Option<String>>(22)?,
                    r.get::<_, Option<String>>(23)?,
                    r.get::<_, Option<i64>>(24)?,
                    r.get::<_, Option<String>>(25)?,
                    r.get::<_, Option<String>>(26)?,
                    r.get::<_, Option<String>>(27)?,
                    r.get::<_, Option<String>>(28)?,
                    r.get::<_, Option<String>>(29)?,
                    r.get::<_, Option<String>>(30)?,
                    r.get::<_, Option<i64>>(31)?,
                    r.get::<_, Option<i64>>(32)?,
                    r.get::<_, Option<String>>(33)?,
                    r.get::<_, Option<String>>(34)?,
                    r.get::<_, Option<String>>(35)?,
                    r.get::<_, Option<i64>>(36)?,
                    r.get::<_, Option<i64>>(37)?,
                ))
            })?;
            for row in rows {
                let (
                    oid,
                    parent,
                    class,
                    name,
                    did,
                    ref_id,
                    path,
                    size,
                    ts,
                    date,
                    mime,
                    pn,
                    device,
                    inode,
                    duration,
                    bitrate,
                    resolution,
                    channels,
                    samplerate,
                    container,
                    video,
                    audio,
                    audio_streams,
                    hdr,
                    album_art,
                    detail_title,
                    creator,
                    artist,
                    album,
                    genre,
                    comment,
                    disc,
                    track,
                    album_artist,
                    composer,
                    contributor,
                    rating,
                    rotation,
                ) = row?;
                let path = crate::path_from_db(&path.unwrap_or_default());
                let mime_s = mime.unwrap_or_else(|| "video/x-matroska".into());
                let ext = mime_to_ext(&mime_s);
                let nonempty = |s: Option<String>| s.filter(|v| !v.is_empty());
                let under_series_or_genre = parent == VIDEO_SERIES_ID
                    || parent.starts_with(&format!("{VIDEO_SERIES_ID}$"))
                    || parent == VIDEO_GENRE_ID
                    || parent.starts_with(&format!("{VIDEO_GENRE_ID}$"));
                let title = if under_series_or_genre {
                    nonempty(name.clone()).or_else(|| nonempty(detail_title.clone()))
                } else {
                    nonempty(detail_title.clone()).or_else(|| nonempty(name.clone()))
                }
                .unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("item")
                        .to_string()
                });
                let probe = crate::probe_from_stored(
                    ext,
                    container.as_deref(),
                    video.as_deref(),
                    audio.as_deref(),
                    audio_streams.as_deref(),
                    hdr.as_deref(),
                    resolution.as_deref(),
                );
                let dlna_pn = pn.filter(|s| !s.is_empty()).or_else(|| {
                    crate::dlna_pn_from_probe(
                        &probe.container,
                        &probe.video,
                        &probe.audio,
                        &probe.hdr,
                        probe.width,
                        probe.height,
                    )
                });
                let item = MediaItem {
                    object_id: oid.clone(),
                    parent_id: parent.clone(),
                    detail_id: did,
                    title,
                    class,
                    date: date.unwrap_or_default(),
                    path: path.clone(),
                    mime: mime_s,
                    ext: ext.into(),
                    size: size.unwrap_or(0) as u64,
                    mtime: ts.unwrap_or(0),
                    captions: caps_by.get(&did).cloned().unwrap_or_default(),
                    probe,
                    dlna_pn,
                    ref_id,
                    device: device.unwrap_or(0) as u64,
                    inode: inode.unwrap_or(0) as u64,
                    duration: duration.filter(|s| !s.is_empty()),
                    bitrate,
                    resolution: resolution.filter(|s| !s.is_empty()),
                    channels,
                    samplerate,
                    album_art: album_art.unwrap_or(0),
                    creator: nonempty(creator),
                    comment: nonempty(comment),
                    artist: nonempty(artist),
                    album_artist: nonempty(album_artist),
                    composer: nonempty(composer),
                    contributor: nonempty(contributor),
                    album: nonempty(album),
                    genre: nonempty(genre),
                    disc,
                    track,
                    rating,
                    rotation,
                    bookmark_sec: 0,
                    watch_count: 0,
                };
                cat.by_detail.entry(did).or_insert_with(|| oid.clone());
                if let Some(p) = cat.containers.get_mut(&parent) {
                    if !p.children.iter().any(|c| c == &oid) {
                        p.children.push(oid.clone());
                    }
                }
                cat.next_detail = cat.next_detail.max(did + 1);
                cat.items.insert(oid, item);
            }
        }
        {
            let mut marks: HashMap<i64, (i64, i64)> = HashMap::new();
            let mut stmt = self
                .conn
                .prepare("SELECT ID, COALESCE(SEC, 0), COALESCE(WATCH_COUNT, 0) FROM BOOKMARKS")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (id, sec, wc) = row?;
                marks.insert(id, (sec, wc));
            }
            for it in cat.items.values_mut() {
                if let Some(&(sec, wc)) = marks.get(&it.detail_id) {
                    it.bookmark_sec = sec;
                    it.watch_count = wc;
                }
            }
        }
        {
            let mut stmt = self.conn.prepare("SELECT ID, PATH FROM ALBUM_ART")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, path) = row?;
                cat.album_art_paths.insert(id, crate::path_from_db(&path));
            }
        }
        cat.ensure_video_folder_mirrors();
        Ok(cat)
    }
}

fn is_virtual_container(id: &str) -> bool {
    matches!(
        id,
        ROOT_ID
            | BROWSEDIR_ID
            | MUSIC_ID
            | MUSIC_ALL_ID
            | MUSIC_GENRE_ID
            | MUSIC_ARTIST_ID
            | MUSIC_ALBUM_ID
            | MUSIC_PLIST_ID
            | MUSIC_DIR_ID
            | MUSIC_CONTRIB_ARTIST_ID
            | MUSIC_ALBUM_ARTIST_ID
            | MUSIC_COMPOSER_ID
            | MUSIC_RATING_ID
            | MUSIC_RECENT_ID
            | VIDEO_ID
            | VIDEO_ALL_ID
            | VIDEO_GENRE_ID
            | VIDEO_ACTOR_ID
            | VIDEO_SERIES_ID
            | VIDEO_PLIST_ID
            | VIDEO_DIR_ID
            | VIDEO_RATING_ID
            | VIDEO_RECENT_ID
            | IMAGE_ID
            | IMAGE_ALL_ID
            | IMAGE_DATE_ID
            | IMAGE_ALBUM_ID
            | IMAGE_CAMERA_ID
            | IMAGE_PLIST_ID
            | IMAGE_DIR_ID
            | IMAGE_RATING_ID
            | IMAGE_RECENT_ID
            | SAMSUNG_AUDIO
            | SAMSUNG_VIDEO
            | SAMSUNG_IMAGE
    )
}

const SCHEMA_VERSION: i64 = 9;

fn table_has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn migrate_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    migrate_schema_inner(conn, None)
}

fn migration_checkpoint(step: u8, fail_after_step: Option<u8>) -> rusqlite::Result<()> {
    if fail_after_step == Some(step) {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ABORT),
            Some(format!("injected migration interruption after step {step}")),
        ));
    }
    Ok(())
}

fn migrate_schema_inner(
    conn: &mut Connection,
    fail_after_step: Option<u8>,
) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let tx = conn.transaction()?;
    if version < 1 {
        for col in ["CONTAINER", "VIDEO", "AUDIO", "AUDIO_STREAMS", "HDR"] {
            if !table_has_column(&tx, "DETAILS", col)? {
                tx.execute(&format!("ALTER TABLE DETAILS ADD COLUMN {col} TEXT"), [])?;
            }
        }
    }
    migration_checkpoint(1, fail_after_step)?;
    if version < 2 {
        tx.execute(
            "DELETE FROM SETTINGS
             WHERE rowid NOT IN (SELECT MAX(rowid) FROM SETTINGS GROUP BY KEY)",
            [],
        )?;
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS IDX_SETTINGS_KEY ON SETTINGS(KEY);
             UPDATE DETAILS SET ALBUM_ART = COALESCE((
               SELECT MIN(a2.ID) FROM ALBUM_ART a2
               WHERE a2.PATH = (SELECT a1.PATH FROM ALBUM_ART a1 WHERE a1.ID = DETAILS.ALBUM_ART)
             ), ALBUM_ART) WHERE ALBUM_ART > 0;
             DELETE FROM ALBUM_ART
             WHERE ID NOT IN (SELECT MIN(ID) FROM ALBUM_ART GROUP BY PATH);
             CREATE UNIQUE INDEX IF NOT EXISTS IDX_ALBUM_ART_PATH ON ALBUM_ART(PATH);
             DELETE FROM PLAYLISTS
             WHERE ID NOT IN (SELECT MIN(ID) FROM PLAYLISTS GROUP BY PATH);
             CREATE UNIQUE INDEX IF NOT EXISTS IDX_PLAYLISTS_PATH ON PLAYLISTS(PATH);",
        )?;
    }
    migration_checkpoint(2, fail_after_step)?;
    if version < 3 {
        tx.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS TR_DETAILS_DELETE
               AFTER DELETE ON DETAILS BEGIN
                 DELETE FROM OBJECTS WHERE DETAIL_ID = OLD.ID;
                 DELETE FROM CAPTIONS WHERE ID = OLD.ID;
                 DELETE FROM BOOKMARKS WHERE ID = OLD.ID;
               END;",
        )?;
    }
    migration_checkpoint(3, fail_after_step)?;
    if version < 5 {
        for (column, kind) in [
            ("ALBUM_ARTIST", "TEXT COLLATE NOCASE"),
            ("COMPOSER", "TEXT COLLATE NOCASE"),
            ("CONTRIBUTOR", "TEXT COLLATE NOCASE"),
            ("RATING", "INTEGER"),
        ] {
            if !table_has_column(&tx, "DETAILS", column)? {
                tx.execute(
                    &format!("ALTER TABLE DETAILS ADD COLUMN {column} {kind}"),
                    [],
                )?;
            }
        }
    }
    if version < 6 {
        for column in ["DEVICE", "INODE"] {
            if !table_has_column(&tx, "PLAYLISTS", column)? {
                tx.execute(
                    &format!("ALTER TABLE PLAYLISTS ADD COLUMN {column} INTEGER DEFAULT 0"),
                    [],
                )?;
            }
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS PLAYLIST_ITEMS (
               PLAYLIST_ID INTEGER NOT NULL REFERENCES PLAYLISTS(ID) ON DELETE CASCADE,
               DETAIL_ID INTEGER NOT NULL REFERENCES DETAILS(ID) ON DELETE CASCADE,
               POSITION INTEGER NOT NULL,
               PRIMARY KEY (PLAYLIST_ID, POSITION));
             CREATE INDEX IF NOT EXISTS IDX_PLAYLIST_ITEMS_DETAIL
               ON PLAYLIST_ITEMS(DETAIL_ID);
             DELETE FROM PLAYLISTS WHERE INODE != 0 AND ID NOT IN (
               SELECT MIN(ID) FROM PLAYLISTS WHERE INODE != 0 GROUP BY DEVICE, INODE
             );
             CREATE UNIQUE INDEX IF NOT EXISTS IDX_PLAYLISTS_DEVICE_INODE
               ON PLAYLISTS(DEVICE, INODE) WHERE INODE != 0;",
        )?;
    }
    if version < 7 && !table_has_column(&tx, "DETAILS", "AUDIO_STREAMS")? {
        tx.execute("ALTER TABLE DETAILS ADD COLUMN AUDIO_STREAMS TEXT", [])?;
    }
    if version < 8 && !table_has_column(&tx, "DETAILS", "STREAM_PROBE_REV")? {
        tx.execute(
            "ALTER TABLE DETAILS ADD COLUMN STREAM_PROBE_REV INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        // Existing populated rows have already survived a probe under the
        // previous schema. Keep genuinely empty rows eligible for the one-time
        // startup backfill.
        tx.execute(
            "UPDATE DETAILS SET STREAM_PROBE_REV = 3
             WHERE NULLIF(HDR, '') IS NOT NULL
                OR NULLIF(DURATION, '') IS NOT NULL
                OR NULLIF(CONTAINER, '') IS NOT NULL
                OR NULLIF(RESOLUTION, '') IS NOT NULL",
            [],
        )?;
    }
    if version < 9 {
        if !table_has_column(&tx, "BOOKMARKS", "UPDATED_AT")? {
            tx.execute(
                "ALTER TABLE BOOKMARKS ADD COLUMN UPDATED_AT INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        // Give every pre-upgrade bookmark a full retention window. Using the
        // upgrade time avoids deleting valid Kodi positions merely because
        // the old schema could not record their last update time.
        tx.execute(
            "UPDATE BOOKMARKS SET UPDATED_AT = CAST(strftime('%s', 'now') AS INTEGER)
             WHERE UPDATED_AT = 0",
            [],
        )?;
    }
    let rev: i64 = tx
        .query_row(
            "SELECT VALUE FROM SETTINGS WHERE KEY = 'stream_probe_rev' LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if rev < 3 {
        // Re-read with libav (rev 3). Earlier revs used a hand-rolled parser.
        tx.execute(
            "UPDATE DETAILS SET HDR = NULL, VIDEO = NULL, AUDIO = NULL,
                 AUDIO_STREAMS = NULL, CONTAINER = NULL, STREAM_PROBE_REV = 0",
            [],
        )?;
        tx.execute(
            "INSERT INTO SETTINGS (KEY, VALUE) VALUES ('stream_probe_rev', '3')
             ON CONFLICT(KEY) DO UPDATE SET VALUE = excluded.VALUE",
            [],
        )?;
    }
    migration_checkpoint(4, fail_after_step)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn verify_integrity(conn: &Connection) -> rusqlite::Result<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(result),
        ))
    }
}

pub fn mime_to_ext(mime: &str) -> &'static str {
    rusty_dlna_protocol::MEDIA_FORMATS
        .iter()
        .find(|format| {
            [format.video_mime, format.audio_mime, format.image_mime]
                .into_iter()
                .flatten()
                .any(|candidate| candidate.eq_ignore_ascii_case(mime))
        })
        .map(|format| format.extension)
        .unwrap_or("dat")
}

#[cfg(test)]
mod query_tests {
    use super::*;

    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "rusty-dlna-db-{name}-{}-{sequence}",
                std::process::id()
            )))
        }
    }

    impl std::ops::Deref for TempDb {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<Path> for TempDb {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = self.0.as_os_str().to_os_string();
                sidecar.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(sidecar));
            }
        }
    }

    fn temp_db(name: &str) -> TempDb {
        TempDb::new(name)
    }

    fn create_legacy_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE OBJECTS (
               ID INTEGER PRIMARY KEY AUTOINCREMENT, OBJECT_ID TEXT UNIQUE NOT NULL,
               PARENT_ID TEXT NOT NULL, REF_ID TEXT, CLASS TEXT NOT NULL,
               DETAIL_ID INTEGER, NAME TEXT);
             CREATE TABLE DETAILS (
               ID INTEGER PRIMARY KEY AUTOINCREMENT, PATH TEXT, SIZE INTEGER,
               TIMESTAMP INTEGER, TITLE TEXT, DURATION TEXT, BITRATE INTEGER,
               SAMPLERATE INTEGER, CREATOR TEXT, ARTIST TEXT, ALBUM TEXT,
               GENRE TEXT, COMMENT TEXT, CHANNELS INTEGER, DISC INTEGER,
               TRACK INTEGER, DATE DATE, RESOLUTION TEXT, THUMBNAIL BOOL DEFAULT 0,
               ALBUM_ART INTEGER DEFAULT 0, ROTATION INTEGER, DLNA_PN TEXT,
               MIME TEXT, DEVICE INTEGER, INODE INTEGER);
             CREATE TABLE ALBUM_ART (ID INTEGER PRIMARY KEY AUTOINCREMENT, PATH TEXT NOT NULL);
             CREATE TABLE CAPTIONS (ID INTEGER NOT NULL, PATH TEXT NOT NULL, PRIMARY KEY (ID, PATH));
             CREATE TABLE BOOKMARKS (ID INTEGER PRIMARY KEY, SEC INTEGER, WATCH_COUNT INTEGER);
             CREATE TABLE PLAYLISTS (
               ID INTEGER PRIMARY KEY AUTOINCREMENT, NAME TEXT NOT NULL, PATH TEXT NOT NULL,
               ITEMS INTEGER DEFAULT 0, FOUND INTEGER DEFAULT 0, TIMESTAMP INTEGER DEFAULT 0);
             CREATE TABLE SETTINGS (KEY TEXT NOT NULL, VALUE TEXT);
             PRAGMA user_version=0;",
        )
        .unwrap();
    }

    fn query_fixture() -> LibraryDb {
        let db = LibraryDb::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, NAME) VALUES ('0', '-1', 'container.storageFolder', 'root')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, NAME) VALUES ('2', '0', 'container.storageFolder', 'Video')",
                [],
            )
            .unwrap();
        for (id, title, track) in [(1, "Zulu", 3), (2, "Alpha", 1), (3, "Middle", 2)] {
            db.conn
                .execute(
                    "INSERT INTO DETAILS (ID, TITLE, DATE, ALBUM, TRACK, CREATOR, MIME) VALUES (?1, ?2, '2024-01-01', 'Album', ?3, 'Creator', 'video/mp4')",
                    params![id, title, track],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID, NAME) VALUES (?1, '2', 'item.videoItem', ?2, ?3)",
                    params![format!("2${id}"), id, format!("file-{id}")],
                )
                .unwrap();
        }
        db
    }

    #[test]
    fn sqlite_catalog_query_counts_sorts_pages_and_handles_huge_offsets() {
        let db = query_fixture();
        let children = db
            .query_children_page(
                "2",
                &[CatalogQuerySort {
                    field: CatalogQueryField::Title,
                    descending: false,
                }],
                CatalogDefaultOrder::FoldersFirst,
                1,
                1,
            )
            .unwrap();
        assert_eq!(children.total, 3);
        assert_eq!(children.object_ids, ["2$3"]);

        let query = CatalogQuery {
            groups: vec![vec![CatalogQueryClause {
                field: CatalogQueryField::Class,
                op: CatalogQueryOp::DerivedFrom("object.item.videoItem".into()),
            }]],
            sort: vec![CatalogQuerySort {
                field: CatalogQueryField::Track,
                descending: true,
            }],
            default_order: CatalogDefaultOrder::FoldersFirst,
        };
        let page = db.query_search_page("0", &query, 0, 2).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.population, 4);
        assert_eq!(page.object_ids, ["2$1", "2$3"]);

        let past_end = db
            .query_search_page("0", &query, i32::MAX as usize, 4096)
            .unwrap();
        assert_eq!(past_end.total, 3);
        assert!(past_end.object_ids.is_empty());
    }

    #[test]
    fn sqlite_catalog_query_preserves_or_and_exists_semantics() {
        let db = query_fixture();
        let query = CatalogQuery {
            groups: vec![
                vec![CatalogQueryClause {
                    field: CatalogQueryField::Title,
                    op: CatalogQueryOp::Contains("alpha".into()),
                }],
                vec![
                    CatalogQueryClause {
                        field: CatalogQueryField::Title,
                        op: CatalogQueryOp::Equals("Zulu".into()),
                    },
                    CatalogQueryClause {
                        field: CatalogQueryField::RefId,
                        op: CatalogQueryOp::Exists(false),
                    },
                ],
            ],
            sort: vec![CatalogQuerySort {
                field: CatalogQueryField::Title,
                descending: false,
            }],
            default_order: CatalogDefaultOrder::FoldersFirst,
        };
        let page = db.query_search_page("2", &query, 0, 10).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.object_ids, ["2$2", "2$1"]);
    }

    #[test]
    fn sqlite_catalog_query_implements_negative_and_relational_operators() {
        let db = query_fixture();
        let cases = [
            (
                CatalogQueryOp::DoesNotContain("u".into()),
                vec!["2$2", "2$3"],
            ),
            (
                CatalogQueryOp::NotEquals("Middle".into()),
                vec!["2$2", "2$1"],
            ),
            (
                CatalogQueryOp::LessThan {
                    value: "Middle".into(),
                    inclusive: true,
                },
                vec!["2$2", "2$3"],
            ),
            (
                CatalogQueryOp::GreaterThan {
                    value: "Middle".into(),
                    inclusive: false,
                },
                vec!["2$1"],
            ),
        ];
        for (op, expected) in cases {
            let query = CatalogQuery {
                groups: vec![vec![
                    CatalogQueryClause {
                        field: CatalogQueryField::Class,
                        op: CatalogQueryOp::Equals("item.videoItem".into()),
                    },
                    CatalogQueryClause {
                        field: CatalogQueryField::Title,
                        op,
                    },
                ]],
                sort: vec![CatalogQuerySort {
                    field: CatalogQueryField::Title,
                    descending: false,
                }],
                default_order: CatalogDefaultOrder::FoldersFirst,
            };
            let page = db.query_search_page("2", &query, 0, 10).unwrap();
            assert_eq!(page.object_ids, expected);
        }
    }

    #[test]
    fn legacy_schema_migrates_atomically_and_deduplicates_constraints() {
        let path = temp_db("legacy-migrate");
        {
            let conn = Connection::open(&path).unwrap();
            create_legacy_schema(&conn);
            conn.execute_batch(
                "INSERT INTO SETTINGS VALUES ('updateID', '4');
                 INSERT INTO SETTINGS VALUES ('updateID', '7');
                 INSERT INTO ALBUM_ART (ID, PATH) VALUES (10, '/art/cover.jpg');
                 INSERT INTO ALBUM_ART (ID, PATH) VALUES (11, '/art/cover.jpg');
                 INSERT INTO DETAILS (ID, PATH, TITLE, ALBUM_ART, MIME)
                   VALUES (1, '/media/a.mkv', 'A', 11, 'video/x-matroska');
                 INSERT INTO BOOKMARKS (ID, SEC, WATCH_COUNT) VALUES (1, 120, 3);
                 INSERT INTO PLAYLISTS (NAME, PATH) VALUES ('old', '/lists/a.m3u');
                 INSERT INTO PLAYLISTS (NAME, PATH) VALUES ('new', '/lists/a.m3u');",
            )
            .unwrap();
        }

        let db = LibraryDb::open(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(table_has_column(&db.conn, "DETAILS", "HDR").unwrap());
        assert!(table_has_column(&db.conn, "BOOKMARKS", "UPDATED_AT").unwrap());
        let bookmark_updated_at: i64 = db
            .conn
            .query_row("SELECT UPDATED_AT FROM BOOKMARKS WHERE ID=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            bookmark_updated_at > 0,
            "pre-upgrade bookmarks must receive the migration time"
        );
        assert_eq!(db.setting("updateID").unwrap().as_deref(), Some("7"));
        assert_eq!(db.detail_album_art(1).unwrap(), 10);
        let art_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM ALBUM_ART", [], |row| row.get(0))
            .unwrap();
        let playlist_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM PLAYLISTS", [], |row| row.get(0))
            .unwrap();
        assert_eq!(art_count, 1);
        assert_eq!(playlist_count, 1);
        assert!(db
            .conn
            .execute("INSERT INTO SETTINGS VALUES ('updateID', '8')", [])
            .is_err());
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn interrupted_migration_rolls_back_every_step_and_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_legacy_schema(&conn);
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO SETTINGS VALUES ('key', 'old');
             INSERT INTO SETTINGS VALUES ('key', 'new');",
        )
        .unwrap();

        let error = migrate_schema_inner(&mut conn, Some(2)).unwrap_err();
        assert!(error
            .to_string()
            .contains("injected migration interruption"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        assert!(!table_has_column(&conn, "DETAILS", "CONTAINER").unwrap());
        let duplicates: i64 = conn
            .query_row("SELECT COUNT(*) FROM SETTINGS WHERE KEY='key'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(duplicates, 2);

        migrate_schema_inner(&mut conn, None).unwrap();
        assert!(table_has_column(&conn, "DETAILS", "CONTAINER").unwrap());
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn foreign_keys_and_delete_relationships_are_enforced() {
        let db = LibraryDb::open_memory().unwrap();
        let foreign_keys: i64 = db
            .conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        assert!(db
            .conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID)
                 VALUES ('bad', '0', 'item.videoItem', 999)",
                [],
            )
            .is_err());
        db.conn
            .execute(
                "INSERT INTO DETAILS (ID, PATH, MIME) VALUES (1, '/a', 'video/mp4')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID)
                 VALUES ('ok', '0', 'item.videoItem', 1)",
                [],
            )
            .unwrap();
        db.conn
            .execute("INSERT INTO CAPTIONS (ID, PATH) VALUES (1, '/a.srt')", [])
            .unwrap();
        db.conn
            .execute("INSERT INTO BOOKMARKS (ID, SEC) VALUES (1, 12)", [])
            .unwrap();
        db.conn
            .execute("DELETE FROM DETAILS WHERE ID=1", [])
            .unwrap();
        for table in ["OBJECTS", "CAPTIONS", "BOOKMARKS"] {
            let count: i64 = db
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} relationship was not deleted");
        }
    }

    #[test]
    fn busy_and_full_failures_roll_back_coherent_updates() {
        let path = temp_db("busy-rollback");
        let primary = LibraryDb::open(&path).unwrap();
        primary
            .conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, NAME)
                 VALUES ('sentinel', '0', 'container.storageFolder', 'keep')",
                [],
            )
            .unwrap();
        let contender =
            LibraryDb::open_with_busy_timeout(&path, std::time::Duration::from_millis(1)).unwrap();
        primary.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let tx = contender.transaction().unwrap();
        let busy = contender.clear_objects().unwrap_err();
        assert!(matches!(
            busy,
            rusqlite::Error::SqliteFailure(ref error, _)
                if error.extended_code & 0xff == rusqlite::ffi::SQLITE_BUSY
                    || error.extended_code & 0xff == rusqlite::ffi::SQLITE_LOCKED
        ));
        drop(tx);
        primary.conn.execute_batch("ROLLBACK").unwrap();
        assert!(primary.object_exists("sentinel").unwrap());
        drop(contender);
        drop(primary);
        let _ = std::fs::remove_file(&path);

        let full = LibraryDb::open_memory().unwrap();
        full.conn
            .execute(
                "INSERT INTO OBJECTS (OBJECT_ID, PARENT_ID, CLASS, NAME)
                 VALUES ('sentinel', '0', 'container.storageFolder', 'keep')",
                [],
            )
            .unwrap();
        let pages: i64 = full
            .conn
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        full.conn
            .pragma_update(None, "max_page_count", pages + 1)
            .unwrap();
        let tx = full.transaction().unwrap();
        full.clear_objects().unwrap();
        let no_space = full
            .conn
            .execute(
                "INSERT INTO DETAILS (COMMENT) VALUES (zeroblob(1048576))",
                [],
            )
            .unwrap_err();
        assert!(matches!(
            no_space,
            rusqlite::Error::SqliteFailure(ref error, _)
                if error.extended_code & 0xff == rusqlite::ffi::SQLITE_FULL
        ));
        drop(tx);
        assert!(full.object_exists("sentinel").unwrap());
    }

    #[test]
    fn database_parent_io_error_is_observable() {
        let parent = temp_db("io-parent");
        std::fs::write(&parent, b"not a directory").unwrap();
        let error = match LibraryDb::open(&parent.join("files.db")) {
            Ok(_) => panic!("database unexpectedly opened below a regular file"),
            Err(error) => error,
        };
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        let _ = std::fs::remove_file(parent);
    }
}
