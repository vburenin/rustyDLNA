# Operations and capacity planning

## Storage budget

Keep `cache_dir` and `db_dir` on local, writable storage. The shipped defaults
share one filesystem, so the capacity floor must cover all of these at once:

- `files.db`, its WAL/SHM files, and enough temporary room for a rebuild;
- scanner artwork below `db_dir/art`;
- on-demand JPEGs below `cache_dir/derived-images`;
- completed fragmented MP4 files, their stamps, and one growing `.part` per
  active remux/transcode job; and
- the configured minimum-free-space reserve.

The defaults cap derived JPEGs at 512 MiB for 30 days and completed transcodes
at 50 GiB for 30 days. `cache_min_free_mb = 128` is a hard shared reserve, not
extra capacity: image requests return 507 and transcode work fails when cleanup
cannot restore it. Protected, actively written transcodes cannot be evicted, so
budget at least the largest possible simultaneous outputs in addition to the
reserve. For a large deployment, start with free space of at least twice the
measured database plus artwork size, both cache quotas, the reserve, and one
worst-case output per `transcode.max_jobs`, then adjust from observed peaks.

Relevant settings are:

```toml
derived_image_cache_mb = 512
derived_image_cache_age_days = 30
cache_min_free_mb = 128

[transcode]
cache_max_mb = 51200
cache_max_age_days = 30
max_jobs = 16
```

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
are exposed alongside their components. Inotify reports only an overflow
marker, not the exact number of kernel events lost, so every increment of
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

## Graceful shutdown budget

`shutdown_timeout_secs` defines the whole-process SIGTERM/SIGINT budget and
defaults to 15 seconds (valid range 2–120). Cancellation is broadcast to the
scanner, inotify/periodic reconciliation, derived-image work, and remux jobs
before any subsystem is joined. SSDP byebye is sent immediately after that
broadcast so it is not delayed by storage or helper cleanup.

Scanner transactions check cancellation before commit and roll back when an
in-flight walk, probe, thumbnail, overflow reconcile, or publication is
stopped. Cancellation-aware SQLite opens use a 250 ms busy window. External
FFmpeg/FFprobe/dovi helpers run in private process groups: shutdown sends TERM,
allows 200 ms for cooperative exit, then sends KILL to any remaining group and
reaps the leader. Atomic image and transcode staging files are removed on the
error path.

The structured shutdown log includes `duration_ms`, `budget_ms`,
`deadline_exceeded`, and `jobs_remaining`. Alert on either of the last two
being nonzero/true. Keep an outer supervisor deadline above the application
budget; the shipped systemd unit uses `TimeoutStopSec=45s` for the default
15-second application budget.

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
the mounted TOML is kept, so clients still see the same UDN. Bind-mounted
caches are refused; delete those host files yourself. Raise
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
