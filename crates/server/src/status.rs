//! MiniDLNA-style `/` and `/status` presentation HTML.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use crate::App;

pub fn status_html(app: &App) -> String {
    let cat = app.catalog.read().expect("catalog");
    let mut audio = 0u32;
    let mut video = 0u32;
    let mut image = 0u32;
    let mut video_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut video_art = 0u32;
    let mut captions = 0u32;
    let mut seen_details: HashSet<i64> = HashSet::new();
    for it in cat.items.values() {
        if !seen_details.insert(it.detail_id) {
            continue;
        }
        captions += it.captions.len() as u32;
        if it.mime.starts_with("audio/") {
            audio += 1;
        } else if it.mime.starts_with("video/") {
            video += 1;
            video_inodes.insert((it.device, it.inode));
            if it.album_art > 0 {
                video_art += 1;
            }
        } else if it.mime.starts_with("image/") {
            image += 1;
        }
    }
    let update_id = app.update_id.load(Ordering::Relaxed);
    let db = app
        .scan_cfg
        .db_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ":memory:".into());
    let cache_n = app.client_cache.lock().map(|c| c.len()).unwrap_or(0);
    let scan_status = app.cache_dir.join("scan.status");
    let scanning = std::fs::read_to_string(&scan_status).unwrap_or_default();
    let remux_n = app.remuxes.lock().map(|m| m.len()).unwrap_or(0);
    let max_jobs = app.cfg.transcode.max_jobs;
    let remux_lines: String = app
        .remuxes
        .lock()
        .map(|m| {
            m.iter()
                .map(|(id, _)| format!("<li>remux detail {id}</li>"))
                .collect()
        })
        .unwrap_or_default();

    let mut body = format!(
        "<html><head><title>{}</title>\
         <meta http-equiv=\"refresh\" content=\"20\"></head><body>\
         <h1>{}</h1>\
         <table>\
         <tr><td>Audio</td><td>{audio}</td></tr>\
         <tr><td>Video</td><td>{video}</td></tr>\
         <tr><td>Image</td><td>{image}</td></tr>\
         <tr><td>Video inodes</td><td>{}</td></tr>\
         <tr><td>Videos with album art</td><td>{video_art}</td></tr>\
         <tr><td>Captions</td><td>{captions}</td></tr>\
         <tr><td>UpdateID</td><td>{update_id}</td></tr>\
         <tr><td>Database</td><td>{}</td></tr>\
         <tr><td>Client cache</td><td>{cache_n}</td></tr>\
         <tr><td>Remux max_jobs</td><td>{max_jobs}</td></tr>\
         <tr><td>Remux active</td><td>{remux_n}</td></tr>\
         </table>",
        html_esc(&app.cfg.friendly_name),
        html_esc(&app.cfg.friendly_name),
        video_inodes.len(),
        html_esc(&db),
    );
    if !scanning.trim().is_empty() {
        body.push_str(&format!(
            "<p>Scanning: {}</p>",
            html_esc(scanning.trim())
        ));
    }
    if remux_n > 0 {
        body.push_str("<ul>");
        body.push_str(&remux_lines);
        body.push_str("</ul>");
    }
    body.push_str("</body></html>");
    body
}

fn html_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}
