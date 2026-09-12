# Operations and capacity planning

## Storage budget

Keep `cache_dir` and `db_dir` on local, writable storage. The shipped defaults
share one filesystem, so the capacity floor must cover all of these at once:

- `files.db`, its WAL/SHM files, and enough temporary room for a rebuild;
- one private scanner stage approximately the size of `files.db`, another
  database-sized allowance for persistent changed-key journal B-tree
  high-water pages after a large rebuild, and short-lived WAL/SHM/journal
  sidecars during preparation;
- scanner artwork below `db_dir/art`;
- on-demand JPEGs below `cache_dir/derived-images`;
- completed fragmented MP4 files, their stamps, and active staging files. A
  Profile-8 job can temporarily hold an extracted HEVC stream, its converted
  Profile-8 stream, a wrapped MP4, and the growing final `.part`; consumed
  stages are deleted promptly, but capacity must cover their worst-case
  overlap; and
- the configured minimum-free-space reserve.

The defaults cap derived JPEGs at 512 MiB for 30 days and completed transcodes
at 50 GiB for 30 days. `cache_min_free_mb = 128` is a hard shared reserve, not
extra capacity: image requests return 507 and transcode work fails when cleanup
cannot restore it. Protected, actively written transcodes cannot be evicted, so
budget at least the largest possible simultaneous outputs and Profile-8 staging
overlaps in addition to the reserve. For a large deployment, start with free
space of at least twice the measured database plus artwork size, both cache
quotas, the reserve, and one worst-case Profile-8 staging footprint per
`transcode.max_jobs`, then adjust from observed peaks.

Relevant settings are:

```toml
derived_image_cache_mb = 512
derived_image_cache_age_days = 30
derived_image_quality = 8 # FFmpeg qscale; 2 is highest quality
cache_min_free_mb = 128

[transcode]
cache_max_mb = 51200
cache_max_age_days = 30
max_jobs = 16
```

Startup maintains both derived-image and completed-transcode caches before it
serves requests. Their quota and retention settings are therefore validated
even when `transcode.enable = false`; an invalid value stops startup before
cache maintenance runs.

The browser-only gateway compresses proxied JSON, JavaScript, and CSS when the
client advertises gzip support. Media and JPEG responses remain uncompressed;
browser cards request scanner-prepared source artwork through a four-request
viewport queue. Preparing 360x540 JPEG posters avoids request-time image work.

Optional `.rusty_previews/<video-stem>/` timeline sidecars live in each source
directory and are operator-generated, not part of `cache_dir` or either cache
quota. Budget media-root capacity separately for up to 256 JPEG sheets per
title; higher requested frame resolutions generally create more sheets.
rustyDLNA opens these files read-only, accepts any manifest layout within the
protocol bounds, ignores incomplete/stale sets, and never starts FFmpeg to
create or repair them during startup or a web request. Publish sheets before
the manifest as specified in the protocol contract. The operator programs that
write those sidecars, Kodi-style NFO files, posters, and generated `genres/`
views live in `contrib/library/`. Point them at the media tree with `--root`
or `RUSTY_DLNA_MEDIA`; caches and IMDb dumps stay in
`<library>/.rusty-library/`. rustyDLNA does not run them, and they must not
embed TMDB or OMDb credentials.

Operator intake and Profile 7 archiving use atomic no-replace moves, including
rollback and privileged retries. Occupied destinations (even dangling symlinks
or concurrent arrivals) are preserved; cross-device and unsupported native
rename operations fail without moving the source. A rollback collision reports
paths that need manual recovery. Existing Streamer derivatives must pass
compatibility and duration checks before an original is archived. Preview
generation enforces an absolute per-attempt deadline even when progress output
stalls mid-line, with at most five seconds of termination grace before killing
the helper group and reaping its child. See the
[operator guide](../contrib/library/README.md#safe-intake-and-conversion) for
platform requirements and replacement options.

Configured UUID syntax and listen, advertise, and interface selection are
resolved before rustyDLNA creates `files.db` or a persisted UUID, loads the
catalog, or maintains caches. A failure in that pure preflight leaves existing
cache artifacts untouched and creates no new storage state.

`RUSTY_DLNA_HTTP_PORT` and `RUSTY_DLNA_SSDP_PORT` override the HTTP CLI/default
and SSDP default ports, respectively. When present, each must be a decimal
integer from 1 through 65535. An invalid override stops startup with the
variable name and accepted range instead of falling back to a live default.

Before changing limits, record the actual footprint and free space:

```sh
du -sh /var/cache/rusty-dlna
du -sh /var/cache/rusty-dlna/art /var/cache/rusty-dlna/derived-images 2>/dev/null
du -h /var/cache/rusty-dlna/files.db*
df -h /var/cache/rusty-dlna
curl -fsS http://127.0.0.1:8200/api/status
```

Back up the database with SQLite's online backup mechanism or stop the service
before copying `files.db`, `files.db-wal`, and `files.db-shm` together. Always
run `rusty-dlna --config CONFIG --database-check` after restore. Generated image
and transcode files are disposable; the database and persisted UUID are not.

The scanner creates hidden `.rusty-dlna-scan-stage-*` files beside `files.db`.
It reserves each stage with a lifetime advisory lock, reuses it across watcher
batches, and removes it on orderly shutdown. A crash can leave one database-
sized stage family behind; startup removes only an exact same-database stage
whose lock can be acquired, including its `-wal`, `-shm`, and `-journal`
sidecars. An active stage is never selected by PID alone. Online stage backup
is cancellation-aware, yields between page batches, and fails when pages stop
making progress rather than monopolizing live bookmark writers.

A failed staged publication leaves the served catalog unchanged. The watcher
immediately attempts a full reconciliation and continues retrying without
requiring another filesystem event, using bounded exponential backoff. Stage
detach trouble causes the pooled writer connection to reopen; journal reset
and obsolete artwork cleanup happen after the live writer and publication
locks are released.

The ordered startup scan/repair/reconcile/metadata pass also retries from its
current empty-or-existing catalog state with bounded exponential backoff. It
retains scan serialization across retries, starts filesystem watches only
after one complete pass succeeds, and wakes promptly for shutdown. Attached
JPEG artwork is copied from bounded libav header metadata when available;
per-file artwork helper and filesystem failures preserve existing artwork and
do not roll back an otherwise valid catalog reconciliation.

Schema version 10 adds path-local probe-sidecar provenance. The first startup
after upgrade schedules legacy rows for one correctness-first reprobe in the
private scanner stage, even when their old stream-probe revision was current.
Helper admission and scanner preparation remain bounded, but operators should
allow one full reconciliation's media-probe time for this upgrade.

## Alerts

### Health contract

`/health` is a bounded, cached liveness/readiness check. It does not walk the
catalog or transcode cache and it does not run SQLite `quick_check` inline.
The database is checked once during startup; a health/status poll schedules at
most one background refresh after five minutes, and a result older than fifteen
minutes is reported as stale. `/api/status` is the detailed operator endpoint
and includes catalog counts and all telemetry.

The HTTP contract is deliberate:

- `healthy` returns 200 when the accept loop, SSDP, scan/watch, notification,
  database, helper, and enabled remux components are ready;
- `degraded` returns 200 because the server remains usable, but requires an
  operator alert. Examples include loss of one redundant scan worker, low cache
  space, helper saturation, or stale reconcile/integrity freshness; and
- `unhealthy` returns 503 for loss of the HTTP accept loop, a failed database
  integrity result, or complete loss of scan/watch publication.

The Compose healthcheck therefore accepts `healthy` and `degraded`; it marks
only `unhealthy` as failed. Compose restart policy does not itself restart a
container merely because it is unhealthy. The native systemd unit similarly
uses `Restart=on-failure` for process failure and leaves degraded service policy
to monitoring. This avoids restart loops for conditions such as low disk space
that a restart cannot repair.

`/api/status.metrics` contains fixed-cardinality HTTP route/status and SOAP
action/fault counters plus Browse/Search and shutdown-duration histograms.
Scanner backlog and dropped-event markers, GENA queue/failures, helper wait and
saturation, database-pool activity, and transcode cache hit/miss/eviction data
are exposed alongside their components. Browser transcode metrics distinguish
completed/job cache reuse from `web_player.prepared_reuses_total`, which counts
seek generations that reused source sampling and the verified FFmpeg identity.
`web_player.requests_total` counts source generations, not the native-HLS
playlist, init, segment, or range requests made within one generation. Those
resource reattachments also do not count as job/cache reuse or trigger another
cache-maintenance pass while their producer remains active.

`transcode.cache_bytes` reports generated output and staging bytes from an
incremental inventory. Maintenance refreshes active artifacts and the requested
cache candidate; publication transfers staging accounting to the completed path,
and failure/ephemeral cleanup removes deleted bytes. A full reconciliation runs
on demand at most once every 30 seconds, independently of producer count, and
reconciles externally added or deleted completed files. Active producers can grow
between observations. Admission checks actual active sizes and filesystem free
space before initial exposure and again before final publication.

Discovery and unlink I/O run outside the job registry. An eviction must reserve
its exact destination and recheck current registered ownership before unlinking;
a pending attachment reserves its candidate before admission. Active readers,
producers and pending admissions therefore remain protected even when a directory
snapshot is stale. The shared maintenance gate still coordinates image/video
space checks. A slow filesystem can delay the request or producer doing a sweep
and queued maintenance, while active attachments, status and cancellation can
access the registry. Failed maintenance is reported as failure rather than a
successful cached quota check.

Existing maintenance counters remain available; `cache_scans` and
`cache_scan_entries` distinguish full discovery from incremental checks.
`cache_lock_wait`, `cache_registry_wait`, `cache_sweep_duration`, and
`cache_maintenance_duration` expose bounded duration histograms. The explicit
`cargo test -p rusty-dlna cache_scale_benchmark --lib -- --ignored --nocapture`
benchmark creates disposable 100/1k/10k/100k-entry caches with 1/2/8 concurrent
maintenance callers. It reports sample counts and variability; these filesystem
measurements do not establish playback latency improvements.

Completed HLS/MSE indexes have a separate process-local metadata LRU, bounded to
16 entries and 16 MiB. It owns no media descriptors or disk companion files and
does not count as generated disk output. Eviction can remove the media normally;
a later attachment must still pass its completion stamp and exact output-identity
checks. Restart reparses indexes. Streaming and index identity decisions are
recorded in [ADR 0004](adr/0004-streaming-output-and-index-identity.md).

The same object reports separate `startup_to_initial_bytes_ms`,
`startup_to_playlist_ready_ms`, Media Source playlist/init/first-fragment
fetch-and-append phase summaries, `startup_to_canplay_ms`, and
`startup_to_playing_ms`. Browser-reported phases use the server-owned job clock;
the client selects only a fixed event name. Counts may differ: MP4 has no Media
Source phases, and a browser that abandons a generation does not report later
phases.
`startup_to_first_playable_ms` remains as a compatibility alias for
`startup_to_initial_bytes_ms`; its historical name did not measure browser
playability.
Superseded browser generations are normal during seeks and source recovery.
Their pending playlist and fragment requests return `409 transcode_cancelled`
and are logged at debug level; they do not count as producer failures.
Inotify reports only an overflow marker, not the exact number of kernel events
lost, so every increment of
`scanner.dropped_events_total` means one or more events were dropped and a full
reconcile was required. Graceful shutdown duration is also emitted as a
structured `duration_ms` log field because the HTTP endpoint is unavailable
after process exit.

## Large-library reconciliation cadence

`rescan_secs` is the minimum interval for a full root reconciliation; zero
disables the periodic worker. `rescan_max_secs` optionally enables adaptive
backoff and must be zero or at least the minimum. Zero preserves the fixed
legacy cadence. With a maximum configured, an unchanged walk doubles the
interval and a costly walk also targets at most a five-percent reconciliation
duty cycle, capped by the maximum. A detected change or failure resets the next
interval to the minimum.

Normal inotify file bursts remain targeted and do not walk every media root.
Directory topology changes, queue overflow, or a burst of at least 256 unique
paths deliberately trigger a bounded full reconciliation. Current/minimum/
maximum intervals and the next scheduled timestamp are exposed under
`/api/status.scanner`.

New regular files are published after their observed writer closes. The
watcher tracks that pending state by physical `(device, inode)` identity, so a
hard link, rename, or unlink during the write cannot publish a partial alias or
strand the remaining path. Every configured lexical directory alias receives
equivalent subtree watches while physical directories are deduplicated for
cycle detection. Pending identities and path history are bounded; an
ambiguous lifecycle or exhausted history deliberately falls back to a full
reconciliation.

rustyDLNA never writes `/proc/sys/fs/inotify/max_user_watches`. Watch-tree
construction is also capped at 65,536 directories in user space. If startup
reports `ENOSPC`, the watcher exits; the host administrator must raise
`fs.inotify.max_user_watches` and restart rustyDLNA. An atomic rebuild keeps the
current tree live until its replacement is complete, so budget temporary
capacity for both trees: roughly twice the recursively watched directory count
reported as `scanner.watch_count`. If a replacement runs out of watch capacity,
the old tree remains active and rustyDLNA retries the rebuild and full
reconciliation with bounded backoff; raising the host limit lets this retry
recover without a restart.

## Graceful shutdown budget

`shutdown_timeout_secs` defines the whole-process SIGTERM/SIGINT budget and
defaults to 15 seconds (valid range 2–120). Cancellation is broadcast to the
scanner, inotify/periodic reconciliation, derived-image work, remux jobs, and
GENA notification delivery before any subsystem is joined. SSDP byebye is sent
immediately after that broadcast so it is not delayed by storage or helper
cleanup.

Scanner transactions check cancellation before commit and roll back when an
in-flight walk, probe, thumbnail, overflow reconcile, or publication is
stopped. Cancellation-aware SQLite opens use a 250 ms busy window. External
FFmpeg/FFprobe/dovi helpers, including startup capability checks, receive null
stdin, continuously drain memory-bounded diagnostics, and run in private
process groups. Shutdown sends TERM, allows up to 200 ms for cooperative exit,
then sends KILL to any remaining group and reaps the leader. The same cleanup
runs after deadlines, cancellation, polling failures, and panics. Atomic image
and transcode staging files are removed on the error path.

GENA shutdown closes notification admission, discards queued deliveries, and
wakes idle delivery workers. Callback attempts have absolute phase deadlines:
500 ms to connect, two seconds to write the request, and 200 ms to receive the
status line (at most 128 bytes), with a 2.7-second whole-attempt ceiling. Partial
progress never renews a deadline. Failed attempts retain the bounded three-try
policy. Reads/writes check shutdown between waits of at most 50 ms; connect may
take its remaining 500 ms. An in-flight callback cannot retry after cancellation. Completed
workers are reaped within the same shutdown budget. At the shared deadline any
worker that has not exited, including one still inside a bounded socket
operation, is detached from application ownership. It owns only dispatcher
state, and application destruction cannot perform a second, post-deadline join.

Media-helper failures return stable, escaped HTTP error messages. Detailed
helper diagnostics, including source-path context, are written only to the
server logs and are never reflected in response bodies.

The structured shutdown log includes `duration_ms`, `budget_ms`,
`deadline_exceeded`, `jobs_remaining`, and `notifications_stopped`. Alert when
`deadline_exceeded` is true, `jobs_remaining` is nonzero, or
`notifications_stopped` is false. Keep an outer supervisor deadline above the
application budget; the shipped systemd unit uses `TimeoutStopSec=45s` for the
default 15-second application budget.

Poll `/api/status` and alert on these conditions:

- immediately: `status == "unhealthy"`, a failed database check, or HTTP 5xx;
- for two consecutive polls: `status == "degraded"`, `scanner.last_error`, or
  `cache.free_bytes < cache.minimum_free_bytes`;
- any increase: `scanner.overflow_count`, `helpers.rejected_total`,
  `helpers.timed_out_total`, `events.dropped_total`, or
  `transcode.failed_total`/`cancelled_total`;
- sustained saturation: `helpers.active == helpers.max_active`, a nonzero
  helper queue, or transcode jobs remaining active beyond their normal media
  duration; and
- filesystem monitoring: less than the configured reserve plus headroom for
  all active `.part` outputs, or growth that would exhaust storage before the
  next operator response window.

Cleanup warnings containing `cannot satisfy quota/free-space requirement` are
actionable. Check permissions, active protected jobs, quota size, and actual
filesystem availability; do not repeatedly restart, because restart cannot
evict an output that is still needed by a client.

Catalog counts deliberately describe different layers. `physical_inodes` is
the number of distinct `(device, inode)` file identities; `path_aliases` is the
number of additional indexed paths sharing those identities; and
`media_records` is their sum. `audio_records`, `video_records`, and
`image_records` classify those media records. `item_objects` includes canonical
and virtual DIDL items, `container_objects` counts browse containers, and
`total_objects` is the sum of those two object counts. Do not compare a virtual
object count directly with the number of files on disk.

## Persistent-volume ownership

For remote browser access, keep the host-network DLNA service on the LAN and
run `docker-compose.web.yaml` as the public reverse proxy's upstream. This
browser-only gateway has no media/cache mounts and does not expose SSDP, SOAP,
GENA, DLNA media, or device/service descriptions. It defaults to
`127.0.0.1:8201`; set `RUSTY_WEB_BIND_IP` only when the authenticated TLS proxy
runs in another container. Validate the live boundary with:

```sh
./restart-web.sh
```

Do not point an Internet virtual host directly at TCP 8200. The gateway is an
endpoint allowlist, not authentication; TLS and credentials remain required at
the outer proxy.

`restart.sh` initializes Docker volume ownership once and stores a versioned
marker. Subsequent restarts validate the root and marker without walking every
cached file. If files were copied into the volume as another user, force one
repair scan with:

```sh
RUSTY_DLNA_REPAIR_OWNERSHIP=1 ./restart.sh
```

`./restart.sh --clean` stops the container, removes the named cache volume, and
lets `docker compose create` provision a new labeled volume with the same
`RUSTY_DLNA_CACHE_VOLUME` name. That drops `files.db`, artwork, derived images,
transcode outputs, Kodi bookmarks, and the persisted UUID file. A `uuid =` in
the mounted TOML is kept, so clients still see the same UDN. The shipped live
Compose file uses this managed volume. A custom bind mount at
`/var/cache/rusty-dlna` is operator-owned: normal restart volume initialization
does not inspect it, and `--clean` refuses to delete it. A named cache volume
with local bind `driver_opts` is the same class of host path: Compose still
reports a volume, but `volume rm` would leave the files on disk, so `--clean`
refuses rather than pretending the catalog is gone. Validate uid/gid 10001
ownership and delete those host files yourself. Raise
`RUSTY_DLNA_START_TIMEOUT` if the first rescan takes longer than 120 seconds
to become healthy.

`scripts/cache-ownership-benchmark.sh` creates a disposable volume with 50,000
artwork/transcode entries, measures first migration and the marked restart, and
fails if the normal path exceeds two seconds. Override the image, count, or
limit with `RUSTY_DLNA_BENCH_IMAGE`, `RUSTY_DLNA_BENCH_FILES`, and
`RUSTY_DLNA_BENCH_MAX_WARM_MS`.

The 2026-08-19 reference run on Linux x86_64 (32 logical CPUs, Docker 29.7.2)
measured 1,168 ms for the one-time ownership migration and 368 ms for a marked
restart over 50,000 entries. These timings include container startup and are a
regression baseline, not a promise for slower storage; rerun the benchmark on
the deployment host.

## Native systemd service

The official artifact remains the OCI image. For a native Debian installation
built from source with the documented FFmpeg runtime libraries, install the
binary at `/usr/local/bin/rusty-dlna`, the config at
`/etc/rusty-dlna/rusty-dlna.toml`, and media read-only below `/srv/media`.
Create an unprivileged account and install the example unit:

```sh
sudo groupadd --system rusty-dlna
sudo useradd --system --gid rusty-dlna --home-dir /var/cache/rusty-dlna \
  --shell /usr/sbin/nologin rusty-dlna
sudo install -d -o root -g rusty-dlna -m 0750 /etc/rusty-dlna
sudo install -d -o root -g rusty-dlna -m 0750 /srv/media
sudo install -o root -g root -m 0644 contrib/systemd/rusty-dlna.service \
  /etc/systemd/system/rusty-dlna.service
sudo systemd-analyze verify /etc/systemd/system/rusty-dlna.service
sudo systemctl daemon-reload
sudo systemctl enable --now rusty-dlna.service
```

Set `media_dir = ["V,/srv/media"]`, `cache_dir = "/var/cache/rusty-dlna"`,
and `db_dir = "/var/cache/rusty-dlna"`. Grant the `rusty-dlna` group read and
directory-traverse access to the media tree. The unit creates the writable
cache directory, drops capabilities, limits file descriptors/tasks, makes the
host filesystem read-only except for the cache, restarts on failure, and gives
graceful shutdown 45 seconds before systemd kills remaining processes. Hardware
encoding needs a reviewed override for the required `/dev/dri` device and group;
do not disable the other protections wholesale.

## Soak validation

The scheduled GitHub Actions soak runs for five hours, leaving time for setup
and report upload within the [six-hour hosted job limit](https://docs.github.com/en/actions/reference/limits).
Manual workflow runs accept 1–18000 seconds. Each cycle exercises the isolated
socket suite and checks process, thread, descriptor, memory, database, and cache
bounds. The workflow retains `soak-report.tsv` with the source fingerprint,
completed cycles, and measured resource peaks.

For a continuous 24-hour run, use a Linux development host with the prerequisites
in [the command reference](INDEX.md#runtime-and-ci-prerequisites):

```sh
SOAK_SECONDS=86400 SOAK_REPORT=/tmp/rustydlna-soak-report.tsv scripts/soak.sh
```

Keep the source tree unchanged during the run. The script uses temporary test
libraries and isolated ports; it does not exercise the live media library.
Resource sampling follows descendants created by every Linux task, including
daemon and helper processes launched by test worker threads. The one-second
sampling interval can miss processes that start and exit between samples.

`scripts/persistent-soak.mjs` supplements those restart/shutdown cycles with one
daemon and one transcode cache kept alive for the entire workload. Install the
locked npm dependencies and Playwright Chromium and build the pinned server
first. Run this dedicated workload separately from the browser matrix and
performance benchmarks so host contention does not obscure resource trends:

```sh
cargo build --locked -p rusty-dlna
node scripts/persistent-soak.mjs --binary=target/debug/rusty-dlna \
  --seconds=3600 --warmup-seconds=150 --output=/tmp/persistent-soak.json
```

Each cycle publishes a generated temporary media file through the watcher,
replaces the previous cycle's file, and overlaps bounded library/search/folder
queries and artwork requests with actual Chromium playback. Chromium presents
frames from cold and cached H264/AAC MSE output generated from MPEG2/FLAC input,
then presents a frame at a seek target. Cache reuse must increment the server's
counter. A separate generated longer source verifies disconnect/reconnect to the
same live compatible-output producer followed by explicit cancellation and
helper reaping. Normal quota enforcement must evict completed output during the
steady-state portion; the harness never deletes cache entries between cycles.
Only the private generated library is changed. HTTP/SSDP default to test ports
18440/12440, and the daemon starts exactly once.

The output's parent directory must already exist. Use `--help` for duration,
sample interval, query load, ports, cache quota, and
resource limits. Defaults sample once per second, run two query workers and
16 queries plus artwork requests per cycle, use a 24 MiB transcode quota, and
bound daemon RSS at 512 MiB, daemon threads at 256, descriptors at 512, children
at 16, daemon plus helper RSS at 2 GiB, database storage at 128 MiB, and total
cache storage at 48 MiB. The transcode quota accommodates outputs still protected
by the reconnect grace while creating pressure on older completed outputs;
reducing it can correctly reject a producer when every eviction candidate is
still active or protected. Query operations have a five-second deadline; browser
startup/seek observations have a 30-second deadline. At least three cycles must
start after warm-up, and every named workload must be observed during that
steady-state period. Missing decoding, active cancellation, reconnect, eviction,
or sufficient steady-state observations fail the run. There are no automatic
retries or codec/platform skips; missing prerequisites fail explicitly.

The JSON report retains exact runtime tool/package versions, binary and harness
hashes, configuration, per-cycle playback/media events, latencies, cache counters,
and idle resource measurements. A sibling `.samples.jsonl` records busy resource
samples, host CPU/memory pressure, scanner state, cache accounting and observed
resource peaks; `.server.log` retains the last 4 MiB of server diagnostics. Idle
samples require no active helpers/children/intermediates and equality between
the independent completed-media byte count and the server's cache accounting
(validation stamp bytes are reported separately). Steady-state RSS, FD and
thread trends report first/last-third medians and descriptive linear slopes.
Defaults fail median growth above 64 MiB RSS, 16 descriptors or eight threads.
Busy peaks and warm-up observations are reported separately from idle growth.
The default 150-second warm-up covers the two-minute prepared-transcode cache:
each recent browser session can retain its validated source descriptor and
FFmpeg identity while later seeks reuse that preparation. That cache has a
64-entry cap and prunes expired entries during access. Include its warm-up in
comparisons before interpreting growth in source/tool descriptors as a leak.
Failed transcodes or cache maintenance also fail the workload; network failures
retain their nested error causes instead of being retried.

Daemon shutdown must finish without escalation within 20 seconds and leave no captured
daemon, helper or browser PID/start identity alive. The private media/database/
cache tree is removed even after workload failure; report artifacts stay outside
it. The duration controls the activity window after fixture generation and
startup; a cycle already in progress finishes its bounded operations before
shutdown. A short run provides finite trend and workload evidence. Use longer
runs on the target host to investigate sustained growth; a passing short run
does not establish indefinite stability or replace the restart-based soak.
