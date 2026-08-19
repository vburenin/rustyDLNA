//! Strict TOML configuration, persistent identity, and external-tool validation.

use std::path::{Path, PathBuf};

use rusty_dlna_transcode::{validate_remap_rules, AudioAction, RecodeAction, RemapRule};

/// A rejected configuration or unavailable/invalid external tool.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigValidationError(String);

impl From<String> for ConfigValidationError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ConfigValidationError {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

/// Loading failure with the configuration path and original source error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_name")]
    pub friendly_name: String,
    #[serde(default)]
    pub network_interface: Vec<String>,
    #[serde(default)]
    pub media_dir: Vec<String>,
    #[serde(default)]
    pub exclude_dir: Vec<String>,
    #[serde(default)]
    pub exclude_file: Vec<String>,
    /// Scan dot-prefixed files/directories. False by default.
    #[serde(default)]
    pub include_hidden: bool,
    /// Additional artwork basenames; `{stem}` or `%s` expands to media stem.
    #[serde(default)]
    pub album_art_names: Vec<String>,
    #[serde(default = "default_true", alias = "enable_subtitles")]
    pub subtitles: bool,
    #[serde(default = "default_true", alias = "enable_thumbnail")]
    pub thumbnails: bool,
    #[serde(default = "default_thumbnail_width")]
    pub thumbnail_width: u32,
    #[serde(default = "default_thumbnail_quality")]
    pub thumbnail_quality: u8,
    #[serde(default, alias = "enable_thumbnail_filmstrip")]
    pub thumbnail_filmstrip: bool,
    /// Hard limits for on-demand image derivatives and their cache.
    #[serde(default = "default_derived_image_max_dimension")]
    pub derived_image_max_dimension: u32,
    #[serde(default = "default_derived_image_max_pixels")]
    pub derived_image_max_pixels: u64,
    #[serde(default = "default_derived_image_memory_mb")]
    pub derived_image_memory_mb: u64,
    #[serde(default = "default_thumbnail_quality")]
    pub derived_image_quality: u8,
    #[serde(default = "default_scan_timeout")]
    pub derived_image_timeout_secs: u64,
    #[serde(default = "default_derived_image_cache_mb")]
    pub derived_image_cache_mb: u64,
    #[serde(default = "default_derived_image_cache_age_days")]
    pub derived_image_cache_age_days: u32,
    #[serde(default = "default_cache_min_free_mb")]
    pub cache_min_free_mb: u64,
    #[serde(default = "default_scan_timeout")]
    pub scan_command_timeout_secs: u64,
    /// Bounded concurrency for libav probing and thumbnail/artwork preparation.
    #[serde(default = "rusty_dlna_scan::default_scan_workers")]
    pub scan_workers: usize,
    #[serde(default = "default_recent_limit")]
    pub recent_limit: usize,
    /// Optional mtime window; omitted means no age cutoff.
    #[serde(default)]
    pub recent_days: Option<u32>,
    /// Permit symlinks below a media root to expose targets outside all media
    /// roots. Disabled by default; enabling this broadens the serving jail.
    #[serde(default)]
    pub wide_links: bool,
    #[serde(default)]
    pub transcode: TranscodeCfg,
    #[serde(default)]
    pub remap: Vec<RemapRule>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub advertise_ip: Option<String>,
    /// Bind address. Default `0.0.0.0`. Set to a LAN IP (e.g. `192.0.2.20`).
    #[serde(default)]
    pub listen_ip: Option<String>,
    #[serde(default)]
    pub notify_interval: Option<u32>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub root_container: Option<String>,
    /// Database directory. Default: `cache_dir`. File is `files.db`.
    #[serde(default)]
    pub db_dir: Option<String>,
    /// Seconds between library rescans (new/changed/deleted files). 0 = off.
    #[serde(default = "default_rescan")]
    pub rescan_secs: u64,
    /// Keep Kodi resume positions and play counts for this many 24-hour days
    /// since their last update. 0 preserves them indefinitely.
    #[serde(default)]
    pub bookmark_retention_days: u32,
    /// Maximum SOAP/GENA request body accepted before dispatch.
    #[serde(default = "default_request_body_bytes")]
    pub max_request_body_bytes: usize,
    /// Maximum concurrently accepted TCP connections.
    #[serde(default = "default_connections")]
    pub max_connections: usize,
    #[serde(default = "default_header_timeout")]
    pub header_read_timeout_secs: u64,
    #[serde(default = "default_body_timeout")]
    pub body_read_timeout_secs: u64,
    #[serde(default = "default_keepalive_timeout")]
    pub keep_alive_timeout_secs: u64,
    #[serde(default = "default_write_timeout")]
    pub write_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            friendly_name: default_name(),
            network_interface: Vec::new(),
            media_dir: Vec::new(),
            exclude_dir: Vec::new(),
            exclude_file: Vec::new(),
            include_hidden: false,
            album_art_names: Vec::new(),
            subtitles: true,
            thumbnails: true,
            thumbnail_width: default_thumbnail_width(),
            thumbnail_quality: default_thumbnail_quality(),
            thumbnail_filmstrip: false,
            derived_image_max_dimension: default_derived_image_max_dimension(),
            derived_image_max_pixels: default_derived_image_max_pixels(),
            derived_image_memory_mb: default_derived_image_memory_mb(),
            derived_image_quality: default_thumbnail_quality(),
            derived_image_timeout_secs: default_scan_timeout(),
            derived_image_cache_mb: default_derived_image_cache_mb(),
            derived_image_cache_age_days: default_derived_image_cache_age_days(),
            cache_min_free_mb: default_cache_min_free_mb(),
            scan_command_timeout_secs: default_scan_timeout(),
            scan_workers: rusty_dlna_scan::default_scan_workers(),
            recent_limit: default_recent_limit(),
            recent_days: None,
            wide_links: false,
            transcode: TranscodeCfg::default(),
            remap: Vec::new(),
            uuid: None,
            cache_dir: None,
            advertise_ip: None,
            listen_ip: None,
            notify_interval: None,
            serial: None,
            root_container: None,
            db_dir: None,
            rescan_secs: default_rescan(),
            bookmark_retention_days: 0,
            max_request_body_bytes: default_request_body_bytes(),
            max_connections: default_connections(),
            header_read_timeout_secs: default_header_timeout(),
            body_read_timeout_secs: default_body_timeout(),
            keep_alive_timeout_secs: default_keepalive_timeout(),
            write_timeout_secs: default_write_timeout(),
        }
    }
}

fn default_rescan() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_thumbnail_width() -> u32 {
    320
}

fn default_thumbnail_quality() -> u8 {
    2
}

fn default_derived_image_max_dimension() -> u32 {
    4096
}

fn default_derived_image_max_pixels() -> u64 {
    40_000_000
}

fn default_derived_image_memory_mb() -> u64 {
    256
}

fn default_derived_image_cache_mb() -> u64 {
    512
}

fn default_derived_image_cache_age_days() -> u32 {
    30
}

fn default_cache_min_free_mb() -> u64 {
    128
}

fn default_scan_timeout() -> u64 {
    30
}

fn default_recent_limit() -> usize {
    rusty_dlna_protocol::object_id::RECENT_MAX
}

fn default_request_body_bytes() -> usize {
    1024 * 1024
}

fn default_connections() -> usize {
    128
}

fn default_header_timeout() -> u64 {
    10
}

fn default_body_timeout() -> u64 {
    30
}

fn default_keepalive_timeout() -> u64 {
    30
}

fn default_write_timeout() -> u64 {
    60
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscodeCfg {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default = "default_jobs")]
    pub max_jobs: u32,
    /// Absolute wall-clock deadline for one background job.
    #[serde(default = "default_transcode_runtime")]
    pub max_runtime_secs: u64,
    /// Keep producing a cache file after the last HTTP reader disconnects.
    #[serde(default = "default_true")]
    pub continue_after_disconnect: bool,
    #[serde(default = "default_transcode_cache_mb")]
    pub cache_max_mb: u64,
    #[serde(default = "default_transcode_cache_age_days")]
    pub cache_max_age_days: u32,
    #[serde(default = "default_scan_timeout")]
    pub verify_timeout_secs: u64,
}

impl Default for TranscodeCfg {
    fn default() -> Self {
        Self {
            enable: false,
            encoder: default_encoder(),
            max_jobs: default_jobs(),
            max_runtime_secs: default_transcode_runtime(),
            continue_after_disconnect: true,
            cache_max_mb: default_transcode_cache_mb(),
            cache_max_age_days: default_transcode_cache_age_days(),
            verify_timeout_secs: default_scan_timeout(),
        }
    }
}

fn default_name() -> String {
    "rustyDLNA".into()
}
fn default_encoder() -> String {
    "libx264".into()
}
fn default_jobs() -> u32 {
    16
}

fn default_transcode_runtime() -> u64 {
    6 * 60 * 60
}

fn default_transcode_cache_mb() -> u64 {
    50 * 1024
}

fn default_transcode_cache_age_days() -> u32 {
    30
}

pub(crate) fn normalize_uuid(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    let value = value
        .strip_prefix("uuid:")
        .or_else(|| value.strip_prefix("UUID:"))
        .unwrap_or(value);
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !valid {
        return Err(format!(
            "uuid must be 32 hexadecimal digits in 8-4-4-4-12 form, got {raw:?}"
        ));
    }
    Ok(format!("uuid:{}", value.to_ascii_lowercase()))
}

fn generate_uuid_v4() -> Result<String, String> {
    use std::io::Read;

    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| format!("cannot obtain random bytes for UUID: {error}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

pub(crate) fn load_or_create_uuid(
    cache_dir: &Path,
    configured: Option<&str>,
) -> Result<String, String> {
    use std::io::Write;

    if let Some(configured) = configured {
        return normalize_uuid(configured);
    }
    std::fs::create_dir_all(cache_dir).map_err(|error| {
        format!(
            "cannot create cache directory {} for persistent UUID: {error}",
            cache_dir.display()
        )
    })?;
    let path = cache_dir.join("uuid");
    let read_existing = || -> Result<String, String> {
        let stored = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read persisted UUID {}: {error}", path.display()))?;
        normalize_uuid(&stored).map_err(|error| format!("invalid persisted UUID: {error}"))
    };
    if path.exists() {
        return read_existing();
    }

    let generated = generate_uuid_v4()?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = cache_dir.join(format!(".uuid-{}-{nonce}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("cannot create UUID temporary file: {error}"))?;
    file.write_all(generated.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("cannot persist generated UUID: {error}"))?;
    drop(file);
    let linked = std::fs::hard_link(&temporary, &path);
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Ok(()) => Ok(generated),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_existing(),
        Err(error) => Err(format!(
            "cannot atomically persist UUID at {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn validate_http_config(cfg: &Config) -> Result<(), ConfigValidationError> {
    if cfg.friendly_name.trim().is_empty() || cfg.friendly_name.len() > 128 {
        return Err("friendly_name must contain 1 to 128 bytes".into());
    }
    if cfg
        .notify_interval
        .is_some_and(|seconds| !(30..=86_400).contains(&seconds))
    {
        return Err("notify_interval must be between 30 and 86400 seconds".into());
    }
    if cfg.rescan_secs > 31_536_000 {
        return Err("rescan_secs must be 0 or at most 31536000".into());
    }
    if cfg.bookmark_retention_days > 36_500 {
        return Err("bookmark_retention_days must be 0 or at most 36500".into());
    }
    if !(1..=1024).contains(&cfg.transcode.max_jobs) {
        return Err("transcode.max_jobs must be between 1 and 1024".into());
    }
    if cfg
        .root_container
        .as_deref()
        .is_some_and(|value| !matches!(value, "V" | "v" | "2" | "A" | "1" | "I" | "3" | "64"))
    {
        return Err("root_container must be V, A, I, 1, 2, 3, or 64".into());
    }
    if cfg
        .network_interface
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err("network_interface entries must not be empty".into());
    }
    if cfg
        .exclude_dir
        .iter()
        .chain(cfg.exclude_file.iter())
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        return Err("exclusion entries must be non-empty and contain no NUL".into());
    }
    if cfg.max_request_body_bytes == 0
        || cfg.max_request_body_bytes > rusty_dlna_http::MAX_HTTP_BODY
    {
        return Err(format!(
            "max_request_body_bytes must be between 1 and {}",
            rusty_dlna_http::MAX_HTTP_BODY
        )
        .into());
    }
    if !(1..=4096).contains(&cfg.max_connections) {
        return Err("max_connections must be between 1 and 4096".into());
    }
    for (name, seconds) in [
        ("header_read_timeout_secs", cfg.header_read_timeout_secs),
        ("body_read_timeout_secs", cfg.body_read_timeout_secs),
        ("keep_alive_timeout_secs", cfg.keep_alive_timeout_secs),
        ("write_timeout_secs", cfg.write_timeout_secs),
    ] {
        if !(1..=3600).contains(&seconds) {
            return Err(format!("{name} must be between 1 and 3600").into());
        }
    }
    if !(16..=4096).contains(&cfg.thumbnail_width) {
        return Err("thumbnail_width must be between 16 and 4096".into());
    }
    if !(2..=31).contains(&cfg.thumbnail_quality) {
        return Err("thumbnail_quality must be between 2 and 31".into());
    }
    if !(16..=16_384).contains(&cfg.derived_image_max_dimension) {
        return Err("derived_image_max_dimension must be between 16 and 16384".into());
    }
    if !(256..=400_000_000).contains(&cfg.derived_image_max_pixels) {
        return Err("derived_image_max_pixels must be between 256 and 400000000".into());
    }
    if !(16..=2048).contains(&cfg.derived_image_memory_mb) {
        return Err("derived_image_memory_mb must be between 16 and 2048".into());
    }
    if !(2..=31).contains(&cfg.derived_image_quality) {
        return Err("derived_image_quality must be between 2 and 31".into());
    }
    if !(1..=600).contains(&cfg.derived_image_timeout_secs) {
        return Err("derived_image_timeout_secs must be between 1 and 600".into());
    }
    if !(1..=102_400).contains(&cfg.derived_image_cache_mb) {
        return Err("derived_image_cache_mb must be between 1 and 102400".into());
    }
    if !(1..=36_500).contains(&cfg.derived_image_cache_age_days) {
        return Err("derived_image_cache_age_days must be between 1 and 36500".into());
    }
    if cfg.cache_min_free_mb > 1024 * 1024 {
        return Err("cache_min_free_mb must be at most 1048576".into());
    }
    if !(1..=600).contains(&cfg.scan_command_timeout_secs) {
        return Err("scan_command_timeout_secs must be between 1 and 600".into());
    }
    if !(1..=64).contains(&cfg.scan_workers) {
        return Err("scan_workers must be between 1 and 64".into());
    }
    if !(1..=10_000).contains(&cfg.recent_limit) {
        return Err("recent_limit must be between 1 and 10000".into());
    }
    if cfg
        .recent_days
        .is_some_and(|days| !(1..=36_500).contains(&days))
    {
        return Err("recent_days must be between 1 and 36500 when set".into());
    }
    for name in &cfg.album_art_names {
        if name.is_empty()
            || Path::new(name).file_name().and_then(|value| value.to_str()) != Some(name)
        {
            return Err(format!(
                "album_art_names entries must be non-empty basenames, got {name:?}"
            )
            .into());
        }
    }
    validate_remap_rules(&cfg.remap, &cfg.transcode.encoder)?;
    if cfg.transcode.enable {
        if !(1..=86_400).contains(&cfg.transcode.max_runtime_secs) {
            return Err("transcode.max_runtime_secs must be between 1 and 86400".into());
        }
        if !(1..=1_048_576).contains(&cfg.transcode.cache_max_mb) {
            return Err("transcode.cache_max_mb must be between 1 and 1048576".into());
        }
        if !(1..=36_500).contains(&cfg.transcode.cache_max_age_days) {
            return Err("transcode.cache_max_age_days must be between 1 and 36500".into());
        }
        if !(1..=600).contains(&cfg.transcode.verify_timeout_secs) {
            return Err("transcode.verify_timeout_secs must be between 1 and 600".into());
        }
    }
    Ok(())
}

pub(crate) fn command_version(name: &str) -> Result<String, ConfigValidationError> {
    let output = std::process::Command::new(name)
        .arg("-version")
        .output()
        .map_err(|error| format!("required executable {name:?} is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "required executable {name:?} failed its version check with {}",
            output.status
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or(name)
        .to_string())
}

fn hardware_encoder(name: &str) -> bool {
    [
        "_nvenc",
        "_qsv",
        "_vaapi",
        "_amf",
        "_videotoolbox",
        "_v4l2m2m",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

/// Validate external programs and the selected encoder for `--check`.
/// dovi_tool is optional because remux-p8 has a documented HDR10 fallback;
/// its absence is reported explicitly instead of being hidden.
pub fn validate_transcode_tools(
    enabled: bool,
    default_encoder: &str,
    remaps: &[RemapRule],
) -> Result<Vec<String>, ConfigValidationError> {
    if !enabled {
        return Ok(Vec::new());
    }
    validate_remap_rules(remaps, default_encoder)?;
    let ffmpeg = command_version("ffmpeg")?;
    let ffprobe = command_version("ffprobe")?;
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|error| format!("cannot query ffmpeg encoders: {error}"))?;
    if !output.status.success() {
        return Err(format!("ffmpeg -encoders failed with {}", output.status).into());
    }
    let encoder_text = String::from_utf8_lossy(&output.stdout);
    let mut encoders = std::collections::BTreeSet::new();
    let mut audio_encoders = std::collections::BTreeSet::new();
    for rule in remaps {
        match rule.action {
            RecodeAction::Hdr10 => {
                encoders.insert(rule.encoder.as_deref().unwrap_or(default_encoder));
                // With no explicit audio policy, unsupported source audio is
                // converted to AC-3 by the selected plan.
                if rule.audio_out.is_none() {
                    audio_encoders.insert("ac3");
                }
            }
            RecodeAction::RemuxP8 => {
                // dovi_tool is optional, so its deterministic HDR10 fallback
                // is part of the configuration we must validate as well.
                encoders.insert("libx264");
            }
            RecodeAction::AudioAc3 => {
                if rule.audio_out.is_none() {
                    audio_encoders.insert("ac3");
                }
            }
            RecodeAction::Original => {}
        }
        match rule.audio_out {
            Some(AudioAction::ToAc3) => {
                audio_encoders.insert("ac3");
            }
            Some(AudioAction::ToAac) => {
                audio_encoders.insert("aac");
            }
            Some(AudioAction::Copy) | None => {}
        }
    }
    for encoder in encoders.iter().chain(audio_encoders.iter()) {
        if !encoder_text
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(*encoder))
        {
            return Err(format!(
                "required ffmpeg encoder {encoder:?} is not present in `ffmpeg -encoders`"
            )
            .into());
        }
    }
    for encoder in encoders {
        if hardware_encoder(encoder) {
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=size=64x64:duration=0.05",
                    "-frames:v",
                    "1",
                    "-c:v",
                    encoder,
                    "-f",
                    "null",
                    "-",
                ])
                .status()
                .map_err(|error| {
                    format!("cannot exercise hardware encoder {encoder:?}: {error}")
                })?;
            if !status.success() {
                return Err(format!(
                    "hardware encoder {encoder:?} is compiled in but unusable; check GPU device mounts, drivers, and container permissions"
                )
                .into());
            }
        }
    }
    let mut notes = vec![format!("{ffmpeg}; {ffprobe}")];
    if remaps
        .iter()
        .any(|rule| rule.action == RecodeAction::RemuxP8)
    {
        match rusty_dlna_transcode::dovi_tool_path() {
            Some(path) => {
                let output = std::process::Command::new(&path)
                    .arg("--version")
                    .output()
                    .map_err(|error| {
                        format!("cannot run dovi_tool at {}: {error}", path.display())
                    })?;
                if !output.status.success() {
                    return Err(format!(
                        "dovi_tool at {} failed its version check with {}",
                        path.display(),
                        output.status
                    )
                    .into());
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let version = stdout.lines().next().unwrap_or("dovi_tool").trim();
                notes.push(format!("{version} ({})", path.display()));
            }
            None => notes.push(
                "WARNING: dovi_tool is not on PATH; remux-p8 will use its HDR10 encode fallback"
                    .into(),
            ),
        }
    }
    Ok(notes)
}

pub fn load_config(path: &Path) -> Result<Config, ConfigLoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigLoadError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

pub fn resolve_http_port(cli: u16) -> u16 {
    std::env::var("RUSTY_DLNA_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(cli)
}

pub fn resolve_ssdp_port() -> u16 {
    std::env::var("RUSTY_DLNA_SSDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(rusty_dlna_protocol::ssdp::SSDP_PORT)
}
