//! rustyDLNA-compatible SQLite store (`scanner_sqlite.h`).
//!
//! Tables: OBJECTS, DETAILS, ALBUM_ART, CAPTIONS, BOOKMARKS, PLAYLISTS,
//! SETTINGS. WAL. On-disk file is `{db_dir}/files.db` (same name The dialect uses).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALL_ID, IMAGE_ALBUM_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, IMAGE_RECENT_ID, MUSIC_ALBUM_ARTIST_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID,
    MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, MUSIC_RECENT_ID,
    ROOT_ID, SAMSUNG_AUDIO, SAMSUNG_IMAGE, SAMSUNG_VIDEO, VIDEO_ACTOR_ID, VIDEO_ALL_ID,
    VIDEO_DIR_ID, VIDEO_GENRE_ID, VIDEO_ID, VIDEO_PLIST_ID, VIDEO_RATING_ID, VIDEO_RECENT_ID,
    VIDEO_SERIES_ID,
};

use crate::{
    path_is_live_file, path_is_unwanted, Caption, Catalog, Container, MediaItem, ScanConfig,
};

/// rustyDLNA schema from `SQLite schema`.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS OBJECTS (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  OBJECT_ID TEXT UNIQUE NOT NULL,
  PARENT_ID TEXT NOT NULL,
  REF_ID TEXT DEFAULT NULL,
  CLASS TEXT NOT NULL,
  DETAIL_ID INTEGER DEFAULT NULL,
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
  ALBUM TEXT COLLATE NOCASE,
  GENRE TEXT COLLATE NOCASE,
  COMMENT TEXT,
  CHANNELS INTEGER,
  DISC INTEGER,
  TRACK INTEGER,
  DATE DATE,
  RESOLUTION TEXT,
  THUMBNAIL BOOL DEFAULT 0,
  ALBUM_ART INTEGER DEFAULT 0,
  ROTATION INTEGER,
  DLNA_PN TEXT,
  MIME TEXT,
  DEVICE INTEGER,
  INODE INTEGER
);
CREATE TABLE IF NOT EXISTS ALBUM_ART (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  PATH TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS CAPTIONS (
  ID INTEGER NOT NULL,
  PATH TEXT NOT NULL,
  PRIMARY KEY (ID, PATH)
);
CREATE TABLE IF NOT EXISTS BOOKMARKS (
  ID INTEGER PRIMARY KEY,
  SEC INTEGER,
  WATCH_COUNT INTEGER
);
CREATE TABLE IF NOT EXISTS PLAYLISTS (
  ID INTEGER PRIMARY KEY AUTOINCREMENT,
  NAME TEXT NOT NULL,
  PATH TEXT NOT NULL,
  ITEMS INTEGER DEFAULT 0,
  FOUND INTEGER DEFAULT 0,
  TIMESTAMP INTEGER DEFAULT 0
);
CREATE TABLE IF NOT EXISTS SETTINGS (
  KEY TEXT NOT NULL,
  VALUE TEXT
);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_INODE ON DETAILS(DEVICE, INODE);
CREATE INDEX IF NOT EXISTS IDX_DETAILS_PATH ON DETAILS(PATH);
CREATE INDEX IF NOT EXISTS IDX_OBJECTS_PARENT ON OBJECTS(PARENT_ID, NAME, OBJECT_ID);
CREATE INDEX IF NOT EXISTS IDX_OBJECTS_DETAIL ON OBJECTS(DETAIL_ID);
"#;

pub struct LibraryDb {
    conn: Connection,
    pub path: PathBuf,
}

impl LibraryDb {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn open_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn begin(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub fn commit(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn find_detail_by_inode(&self, device: i64, inode: i64) -> rusqlite::Result<Option<i64>> {
        Ok(self.find_inode_source(device, inode)?.map(|(id, _, _)| id))
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
    ) -> rusqlite::Result<Option<(i64, i64, String)>> {
        if inode == 0 {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT ID, TIMESTAMP, PATH FROM DETAILS
                 WHERE DEVICE = ?1 AND INODE = ?2 AND MIME IS NOT NULL LIMIT 1",
                params![device, inode],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
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
                CREATOR, ARTIST, ALBUM, GENRE, COMMENT, CHANNELS, DISC, TRACK,
                DATE, RESOLUTION, THUMBNAIL, ALBUM_ART, ROTATION, DLNA_PN, MIME,
                DEVICE, INODE)
             SELECT ?1, ?2, ?3, TITLE, DURATION, BITRATE, SAMPLERATE,
                CREATOR, ARTIST, ALBUM, GENRE, COMMENT, CHANNELS, DISC, TRACK,
                DATE, RESOLUTION, THUMBNAIL, ALBUM_ART, ROTATION, DLNA_PN, MIME,
                ?4, ?5
             FROM DETAILS WHERE ID = ?6",
            params![path, size, mtime, device, inode, src_id],
        )?;
        let id = self.conn.last_insert_rowid();
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM CAPTIONS WHERE ID = ?1",
            [id],
            |r| r.get(0),
        )?;
        if n == 0 {
            self.conn.execute(
                "INSERT OR IGNORE INTO CAPTIONS (ID, PATH)
                 SELECT ?1, PATH FROM CAPTIONS WHERE ID = ?2",
                params![id, src_id],
            )?;
        }
        Ok(id)
    }

    pub fn all_detail_stats(&self) -> rusqlite::Result<Vec<(String, i64, i64, i64, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT PATH, ID, SIZE, TIMESTAMP, DEVICE, INODE FROM DETAILS WHERE PATH IS NOT NULL AND MIME IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn find_detail_by_path(&self, path: &str) -> rusqlite::Result<Option<(i64, i64, i64)>> {
        self.conn
            .query_row(
                "SELECT ID, SIZE, TIMESTAMP FROM DETAILS WHERE PATH = ?1 LIMIT 1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    pub fn insert_detail(
        &self,
        path: &str,
        size: i64,
        timestamp: i64,
        title: &str,
        date: &str,
        mime: &str,
        device: i64,
        inode: i64,
        dlna_pn: Option<&str>,
    ) -> rusqlite::Result<i64> {
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
        self.conn.execute(
            "UPDATE DETAILS SET DURATION = ?1, BITRATE = ?2, RESOLUTION = ?3,
                 CHANNELS = ?4, SAMPLERATE = ?5 WHERE ID = ?6",
            params![duration, bitrate, resolution, channels, samplerate, id],
        )?;
        Ok(())
    }

    pub fn details_missing_av_meta(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ID, PATH FROM DETAILS
             WHERE MIME IS NOT NULL AND PATH IS NOT NULL
               AND (DURATION IS NULL OR DURATION = '')",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn update_detail_stat(&self, id: i64, size: i64, timestamp: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE DETAILS SET SIZE = ?1, TIMESTAMP = ?2 WHERE ID = ?3",
            params![size, timestamp, id],
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

    pub fn replace_captions(&self, detail_id: i64, caps: &[Caption]) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM CAPTIONS WHERE ID = ?1", [detail_id])?;
        for c in caps {
            self.conn.execute(
                "INSERT OR IGNORE INTO CAPTIONS (ID, PATH) VALUES (?1, ?2)",
                params![detail_id, c.path.to_string_lossy().as_ref()],
            )?;
        }
        Ok(())
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

    pub fn object_exists(&self, object_id: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |_| Ok(()),
            )
            .ok()
            .is_some()
    }

    pub fn object_detail_id(&self, object_id: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT DETAIL_ID FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
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
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()?;
        let Some((_, device, inode)) = row else {
            return Ok(0);
        };
        let mut victims: Vec<(i64, String)> = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT ID, PATH FROM DETAILS WHERE DEVICE = ?1 AND INODE = ?2",
            )?;
            let rows = stmt.query_map(params![device, inode], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for r in rows {
                victims.push(r?);
            }
        }
        let mut n = 0usize;
        for (id, p) in victims {
            let gone = p == path || !path_is_live_file(Path::new(&p));
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
            if !path_is_live_file(Path::new(&p)) {
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
            if path_is_unwanted(std::path::Path::new(&p), cfg) {
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
        self.conn
            .query_row("SELECT count(*) FROM OBJECTS WHERE DETAIL_ID IS NOT NULL", [], |r| {
                r.get(0)
            })
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

    pub fn detail_has_object(&self, detail_id: i64) -> bool {
        self.conn
            .query_row(
                "SELECT count(*) FROM OBJECTS WHERE DETAIL_ID = ?1",
                [detail_id],
                |r| r.get::<_, i64>(0),
            )
            .ok()
            .is_some_and(|n| n > 0)
    }

    pub fn detail_count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT count(*) FROM DETAILS WHERE MIME IS NOT NULL", [], |r| {
                r.get(0)
            })
    }

    pub fn get_update_id(&self) -> u32 {
        self.conn
            .query_row(
                "SELECT VALUE FROM SETTINGS WHERE KEY = 'updateID' LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    pub fn set_update_id(&self, id: u32) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM SETTINGS WHERE KEY = 'updateID'", [])?;
        self.conn.execute(
            "INSERT INTO SETTINGS (KEY, VALUE) VALUES ('updateID', ?1)",
            [id.to_string()],
        )?;
        Ok(())
    }

    pub fn bump_update_id(&self) -> rusqlite::Result<u32> {
        let n = self.get_update_id().saturating_add(1);
        self.set_update_id(n)?;
        Ok(n)
    }

    pub fn clear_objects(&self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM OBJECTS", [])?;
        Ok(())
    }

    pub fn seed_virtual_containers(&self) -> rusqlite::Result<()> {
        let rows = [
            (ROOT_ID, "-1", "container.storageFolder", "root"),
            (BROWSEDIR_ID, ROOT_ID, "container.storageFolder", "Browse Folders"),
            (MUSIC_ID, ROOT_ID, "container.storageFolder", "Music"),
            (VIDEO_ID, ROOT_ID, "container.storageFolder", "Video"),
            (IMAGE_ID, ROOT_ID, "container.storageFolder", "Pictures"),
            (VIDEO_ALL_ID, VIDEO_ID, "container.storageFolder", "All Video"),
            (VIDEO_DIR_ID, VIDEO_ID, "container.storageFolder", "Folders"),
            (VIDEO_RECENT_ID, VIDEO_ID, "container.storageFolder", "Recently Added"),
            (MUSIC_ALL_ID, MUSIC_ID, "container.storageFolder", "All Music"),
            (IMAGE_ALL_ID, IMAGE_ID, "container.storageFolder", "All Pictures"),
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
    ) -> bool {
        if inode == 0 {
            return false;
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
            .ok()
            .is_some_and(|n| n > 0)
    }

    /// Same inode+title listed more than once in one folder.
    pub fn folders_have_duplicate_inodes(&self) -> bool {
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
                |r| r.get(0),
            )
            .unwrap_or(false)
    }

    pub fn find_child_object(&self, parent_id: &str, name: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT OBJECT_ID FROM OBJECTS WHERE PARENT_ID = ?1 AND NAME = ?2 LIMIT 1",
                params![parent_id, name],
                |r| r.get(0),
            )
            .ok()
    }

    pub fn object_name(&self, object_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT NAME FROM OBJECTS WHERE OBJECT_ID = ?1",
                [object_id],
                |r| r.get(0),
            )
            .ok()
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
                cat.containers.entry(id.clone()).or_insert_with(|| Container {
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
            let mut stmt = self.conn.prepare("SELECT ID, PATH FROM CAPTIONS ORDER BY PATH")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, p) = row?;
                let ext = crate::caption_ext(&p).to_string();
                let e = caps_by.entry(id).or_default();
                let index = e.len() as u32;
                e.push(Caption {
                    index,
                    path: PathBuf::from(p),
                    ext,
                });
            }
        }
        {
            let mut stmt = self.conn.prepare(
                "SELECT o.OBJECT_ID, o.PARENT_ID, o.CLASS, o.NAME, o.DETAIL_ID, o.REF_ID,
                        d.PATH, d.SIZE, d.TIMESTAMP, d.DATE, d.MIME, d.DLNA_PN,
                        d.DEVICE, d.INODE, d.DURATION, d.BITRATE, d.RESOLUTION,
                        d.CHANNELS, d.SAMPLERATE
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
                ) = row?;
                let path = PathBuf::from(path.unwrap_or_default());
                let mime_s = mime.unwrap_or_else(|| "video/x-matroska".into());
                let ext = mime_to_ext(&mime_s);
                let title = name.unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("item")
                        .to_string()
                });
                let probe = crate::probe_from_name(&title, &path, ext);
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
                    dlna_pn: pn,
                    ref_id,
                    device: device.unwrap_or(0) as u64,
                    inode: inode.unwrap_or(0) as u64,
                    duration: duration.filter(|s| !s.is_empty()),
                    bitrate,
                    resolution: resolution.filter(|s| !s.is_empty()),
                    channels,
                    samplerate,
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

pub fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "video/x-matroska" | "video/x-mkv" => "mkv",
        "video/mp4" => "mp4",
        "video/x-msvideo" => "avi",
        "video/quicktime" => "mov",
        "video/mpeg" | "video/vnd.dlna.mpeg-tts" => "ts",
        "audio/mpeg" => "mp3",
        "audio/x-flac" | "audio/flac" => "flac",
        "image/jpeg" => "jpg",
        _ => "dat",
    }
}
