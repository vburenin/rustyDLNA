//! One background remux per title. Kodi opens several GETs at once;
//! they all attach here. Probe disconnect does not kill ffmpeg.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusty_dlna_http::{
    live_transcode_response, media_response, now_imf_date, parse_byte_range, parse_open_range,
    HttpRequest, HttpResponse, RangeError, RemuxJobSpec,
};
use rusty_dlna_transcode::{
    cache_is_fresh, cache_part, run_remux_p8, write_cache_stamp, RecodeAction, TranscodePlan,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::App;

const FIRST_BYTES: u64 = 16 * 1024;
const FIRST_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

pub struct RemuxJob {
    pub dest: PathBuf,
    pub part: PathBuf,
    pub(crate) failed: Mutex<Option<String>>,
    pub(crate) done: AtomicBool,
    pub(crate) silent_prepass: AtomicBool,
}

impl RemuxJob {
    fn err(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|g| g.clone())
    }
}

fn spawn_ffmpeg(app: Arc<App>, spec: RemuxJobSpec, job: Arc<RemuxJob>) {
    let app_err = app.clone();
    let job_err = job.clone();
    let id_err = spec.detail_id;
    let spawned = std::thread::Builder::new()
        .name(format!("remux-{}", spec.detail_id))
        .spawn(move || {
            let dest = spec.dest.clone();
            let part = job.part.clone();
            let id = spec.detail_id;
            write_cache_stamp(&dest, &spec.src);
            if spec.remux_p8 {
                let p8 = TranscodePlan {
                    action: RecodeAction::RemuxP8,
                    video_encoder: "copy".into(),
                    audio: rusty_dlna_transcode::AudioAction::ToAac,
                    audio_index: spec.audio_index,
                    container: "mp4",
                    ..TranscodePlan::default()
                };
                tracing::info!(id, dest = %dest.display(), "remux-p8 dovi_tool start");
                job.silent_prepass.store(true, Ordering::SeqCst);
                let p8_result = run_remux_p8(&spec.src, &part, &p8);
                job.silent_prepass.store(false, Ordering::SeqCst);
                match p8_result {
                    Ok(()) => {
                        finalize_remux(&job, id, &dest, &part);
                        if !job.done.load(Ordering::SeqCst) {
                            if let Ok(mut map) = app.remuxes.lock() {
                                map.remove(&id);
                            }
                        }
                        app.jobs.release();
                        return;
                    }
                    Err(e) => {
                        tracing::error!(id, dest = %dest.display(), "{e}; falling back to hdr10");
                        let _ = std::fs::remove_file(&part);
                    }
                }
            }
            let args = spec.args;
            tracing::info!(id, dest = %dest.display(), "remux job start");
            let mut cmd = std::process::Command::new(&args[0]);
            cmd.args(&args[1..]);
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::piped());
            let result = cmd.output();
            match result {
                Ok(out) if out.status.success() => {
                    finalize_remux(&job, id, &dest, &part);
                }
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let tail = tail_str(&err, 2000);
                    tracing::error!(
                        id,
                        status = %out.status,
                        dest = %dest.display(),
                        stderr = %tail,
                        "ffmpeg remux failed"
                    );
                    if let Ok(mut g) = job.failed.lock() {
                        *g = Some(format!("ffmpeg {}: {tail}", out.status));
                    }
                    let _ = std::fs::remove_file(&part);
                }
                Err(e) => {
                    tracing::error!(id, dest = %dest.display(), %e, "ffmpeg spawn failed");
                    if let Ok(mut g) = job.failed.lock() {
                        *g = Some(format!("spawn: {e}"));
                    }
                }
            }
            if !job.done.load(Ordering::SeqCst) {
                if let Ok(mut map) = app.remuxes.lock() {
                    map.remove(&id);
                }
            }
            app.jobs.release();
        });
    if let Err(e) = spawned {
        tracing::error!(id = id_err, %e, "remux thread spawn failed");
        if let Ok(mut g) = job_err.failed.lock() {
            *g = Some(format!("thread: {e}"));
        }
        app_err.jobs.release();
    }
}

fn finalize_remux(job: &RemuxJob, id: i64, dest: &Path, part: &Path) {
    let n = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if n == 0 {
        let msg = "ffmpeg produced empty remux".to_string();
        tracing::error!(id, dest = %dest.display(), "{msg}");
        if let Ok(mut g) = job.failed.lock() {
            *g = Some(msg);
        }
        let _ = std::fs::remove_file(part);
        return;
    }
    if let Err(e) = std::fs::rename(part, dest) {
        let msg = format!("remux rename: {e}");
        tracing::error!(id, dest = %dest.display(), "{msg}");
        if let Ok(mut g) = job.failed.lock() {
            *g = Some(msg);
        }
        return;
    }
    tracing::info!(id, dest = %dest.display(), bytes = n, "remux job done");
    job.done.store(true, Ordering::SeqCst);
}

fn tail_str(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        return t.to_string();
    }
    t[t.len() - max..].to_string()
}

/// Start or attach. `started` is true when this call launched ffmpeg.
pub fn attach(app: Arc<App>, spec: RemuxJobSpec) -> Result<Arc<RemuxJob>, String> {
    if cache_is_fresh(&spec.dest, &spec.src) {
        return Ok(Arc::new(RemuxJob {
            dest: spec.dest.clone(),
            part: cache_part(&spec.dest),
            failed: Mutex::new(None),
            done: AtomicBool::new(true),
            silent_prepass: AtomicBool::new(false),
        }));
    }
    if spec.dest.is_file() {
        tracing::info!(
            id = spec.detail_id,
            dest = %spec.dest.display(),
            "stale remux cache, rebuilding"
        );
        let _ = std::fs::remove_file(&spec.dest);
        let _ = std::fs::remove_file(cache_part(&spec.dest));
        let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&spec.dest));
    }
    let mut map = app.remuxes.lock().expect("remuxes");
    if let Some(j) = map.get(&spec.detail_id) {
        if j.err().is_none() {
            tracing::info!(id = spec.detail_id, dest = %spec.dest.display(), "remux attach");
            return Ok(j.clone());
        }
        map.remove(&spec.detail_id);
    }
    if !app.jobs.try_add() {
        return Err(format!(
            "transcode busy (max_jobs={})",
            app.cfg.transcode.max_jobs
        ));
    }
    let part = cache_part(&spec.dest);
    if part.exists() {
        let _ = std::fs::remove_file(&part);
    }
    if let Some(parent) = part.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let job = Arc::new(RemuxJob {
        dest: spec.dest.clone(),
        part: part.clone(),
        failed: Mutex::new(None),
        done: AtomicBool::new(false),
        silent_prepass: AtomicBool::new(false),
    });
    map.insert(spec.detail_id, job.clone());
    drop(map);
    spawn_ffmpeg(app, spec, job.clone());
    Ok(job)
}

pub async fn wait_ready(job: &RemuxJob) -> Result<PathBuf, String> {
    let mut deadline = Instant::now() + FIRST_WAIT;
    loop {
        if job.dest.is_file()
            && job.dest.metadata().map(|m| m.len() > 0).unwrap_or(false)
        {
            return Ok(job.dest.clone());
        }
        if job.part.is_file()
            && job.part.metadata().map(|m| m.len() >= FIRST_BYTES).unwrap_or(false)
        {
            return Ok(job.part.clone());
        }
        if let Some(e) = job.err() {
            return Err(e);
        }
        if job.silent_prepass.load(Ordering::SeqCst) {
            deadline = Instant::now() + FIRST_WAIT;
        } else if Instant::now() > deadline {
            return Err(format!(
                "remux produced no data in {}s",
                FIRST_WAIT.as_secs()
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

pub async fn serve_remux(
    app: &Arc<App>,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    spec: RemuxJobSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let head = req.method.eq_ignore_ascii_case("HEAD");
    let job = match attach(app.clone(), spec.clone()) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(
                id = spec.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "{e}"
            );
            let err = HttpResponse::html(503, "Service Unavailable", &e);
            sock.write_all(&err.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            return Ok(());
        }
    };
    let path = match wait_ready(&job).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                id = spec.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "{e}"
            );
            let err = HttpResponse::html(500, "Internal Server Error", &e);
            sock.write_all(&err.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            return Ok(());
        }
    };
    let finished = path == job.dest
        && job.dest.is_file()
        && job.dest.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if finished {
        return serve_finished(app, sock, req, &job.dest, head).await;
    }
    serve_growing(app, sock, req, &job, head).await
}

async fn serve_finished(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    dest: &Path,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let size = dest.metadata()?.len();
    let range = match req.header("Range") {
        None => None,
        Some(v) => match parse_byte_range(v, size) {
            Ok(r) => r,
            Err(RangeError::Invalid) => {
                tracing::error!(path = %req.path, range = v, "invalid Range");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                sock.write_all(&err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                tracing::error!(path = %req.path, range = v, size, "range past remux EOF");
                let mut err = HttpResponse::html(
                    416,
                    "Requested Range Not Satisfiable",
                    "range past EOF",
                );
                err.set("Content-Range", format!("bytes */{size}"));
                sock.write_all(&err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
        },
    };
    let (start, end) = match range {
        Some(r) => (r.start, r.end),
        None => (0, size.saturating_sub(1)),
    };
    let mut resp = media_response(
        &app.server,
        &now_imf_date(),
        "video/mp4",
        size,
        range,
        Vec::new(),
        None,
        1,
    );
    resp.persist = false;
    if head {
        sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
            .await?;
        return Ok(());
    }
    sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
        .await?;
    crate::stream_file_range(sock, dest, start, end).await?;
    Ok(())
}

async fn serve_growing(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let open = match req.header("Range") {
        None => (0u64, None),
        Some(v) => match parse_open_range(v) {
            Ok(p) => p,
            Err(_) => {
                tracing::error!(path = %req.path, range = v, "invalid Range on growing remux");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                sock.write_all(&err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
        },
    };
    let (start, end) = open;
    let probe = end.is_some_and(|e| e.saturating_sub(start) < 2 * 1024 * 1024);
    if probe {
        if let Some(e) = end {
            if let Err(err) = wait_offset(job, e.saturating_add(1)).await {
                tracing::error!(id = %job.dest.display(), "{err}");
                let resp = HttpResponse::html(500, "Internal Server Error", &err);
                sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
            let have = current_len(job);
            let end = e.min(have.saturating_sub(1));
            let mut resp = live_transcode_response("video/mp4");
            resp.status = 206;
            resp.reason = "OK".into();
            resp.set("Content-Range", format!("bytes {start}-{end}/*"));
            resp.set("Content-Length", end.saturating_sub(start).saturating_add(1));
            sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            if !head {
                stream_growing(sock, job, start, Some(end)).await?;
            }
            return Ok(());
        }
    }
    let resp = live_transcode_response("video/mp4");
    sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
        .await?;
    if head {
        return Ok(());
    }
    stream_growing(sock, job, start, end).await
}

fn current_len(job: &RemuxJob) -> u64 {
    if job.dest.is_file() {
        return job.dest.metadata().map(|m| m.len()).unwrap_or(0);
    }
    job.part.metadata().map(|m| m.len()).unwrap_or(0)
}

fn current_path(job: &RemuxJob) -> PathBuf {
    if job.dest.is_file() {
        job.dest.clone()
    } else {
        job.part.clone()
    }
}

async fn wait_offset(job: &RemuxJob, need: u64) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        if let Some(e) = job.err() {
            return Err(e);
        }
        if current_len(job) >= need || job.done.load(Ordering::SeqCst) && current_len(job) > 0 {
            return Ok(());
        }
        if t0.elapsed() > FIRST_WAIT {
            return Err(format!("remux offset {need} not reached"));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn stream_growing(
    sock: &mut tokio::net::TcpStream,
    job: &Arc<RemuxJob>,
    start: u64,
    end: Option<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut path = current_path(job);
    let mut f = tokio::fs::File::open(&path).await?;
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut pos = start;
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent = 0u64;
    loop {
        if let Some(e) = end {
            if pos > e {
                break;
            }
        }
        if let Some(err) = job.err() {
            if sent == 0 {
                tracing::error!(dest = %path.display(), "{err}");
                return Err(err.into());
            }
            break;
        }
        let size = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => {
                let next = current_path(job);
                if next != path && next.is_file() {
                    path = next;
                    f = tokio::fs::File::open(&path).await?;
                    f.seek(std::io::SeekFrom::Start(pos)).await?;
                    continue;
                }
                if job.done.load(Ordering::SeqCst) {
                    break;
                }
                0
            }
        };
        if pos < size {
            let want = match end {
                Some(e) => (e + 1).saturating_sub(pos).min(size - pos),
                None => size - pos,
            };
            let n = std::cmp::min(buf.len(), want as usize);
            let got = f.read(&mut buf[..n]).await?;
            if got == 0 {
                tokio::time::sleep(POLL).await;
                continue;
            }
            if let Err(e) = sock.write_all(&buf[..got]).await {
                if sent == 0 {
                    tracing::error!(dest = %path.display(), %e, "client dropped before remux bytes");
                    return Err(e.into());
                }
                return Ok(());
            }
            pos += got as u64;
            sent += got as u64;
            continue;
        }
        if job.done.load(Ordering::SeqCst) || job.dest.is_file() && pos >= current_len(job) {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    if sent == 0 {
        tracing::error!(dest = %path.display(), "remux stream sent 0 bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_ready_extends_during_silent_prepass() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-wait-ready-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let dest = tmp.join("out.mp4");
        let part = tmp.join("out.mp4.part");
        let job = Arc::new(RemuxJob {
            dest: dest.clone(),
            part,
            failed: Mutex::new(None),
            done: AtomicBool::new(false),
            silent_prepass: AtomicBool::new(true),
        });
        assert!(!dest.exists());
        let writer = {
            let dest = dest.clone();
            let job = job.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(80)).await;
                std::fs::write(&dest, vec![0u8; FIRST_BYTES as usize]).unwrap();
                job.silent_prepass.store(false, Ordering::SeqCst);
                job.done.store(true, Ordering::SeqCst);
            })
        };
        let t0 = Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(2), wait_ready(&job))
            .await
            .expect("wait_ready must return during silent prepass, not after FIRST_WAIT")
            .expect("wait_ready");
        assert_eq!(got, dest);
        assert!(t0.elapsed() < Duration::from_secs(2));
        writer.await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
