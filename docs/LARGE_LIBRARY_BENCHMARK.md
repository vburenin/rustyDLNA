# Large-library benchmark

`scripts/large-library-benchmark.sh` is the reproducible 50k-file scale
workload. It builds release binaries, creates a disposable sharded library,
starts the daemon, waits for the complete catalog to reach the watcher phase,
and records a JSON report. The working directory is deleted unless
`RUSTY_DLNA_BENCH_KEEP_WORKDIR=1` is set.

Run the reference workload from the repository root:

```sh
scripts/large-library-benchmark.sh
```

The defaults are 50,000 distinct physical files, 5,000 hard-link aliases,
5,000 symlink aliases, 16 scanner workers, and 200 timed requests per SOAP
action. Override them with `RUSTY_DLNA_BENCH_FILES`,
`RUSTY_DLNA_BENCH_ALIASES`, `RUSTY_DLNA_BENCH_SCAN_WORKERS`, and
`RUSTY_DLNA_BENCH_REQUESTS`. `RUSTY_DLNA_BENCH_OUTPUT` selects the report path;
otherwise an ignored timestamped file is written below `benchmark-results/`.

Each physical file is a copy of the tracked, valid
`testdata/library/video/movie.mkv` fixture. Aliases point at the first 5,000
physical files. Thumbnails and subtitles are disabled so the result isolates
filesystem discovery, media probing, SQLite publication, catalog projection,
and SOAP behavior. The resulting reference catalog has 50,000 physical
inodes, 10,000 additional paths, 60,000 media records, 170,000 item objects,
140 containers, and 170,140 total objects.

Cold time starts immediately before daemon launch and ends when `/api/status`
reports the watcher phase, no scanner error, and the exact physical and alias
counts. CPU time, resident/peak memory, open descriptors, and SQLite bytes are
sampled at that point. Reconciliation is run twice against an isolated SQLite
online backup, without loading a second server catalog or changing the daemon's
scan epoch: the first result records any one-time database normalization,
and the second is the steady unchanged measurement. Browse and Search each get
10 warmups followed by 200 sequential requests for a 64-object page over
closed local HTTP connections. Update latency starts before an `fsync` of a new
media file and ends when a targeted inotify update is visible through Browse.

The HTTP check also validates web API schema 2 and measures the first video
page, a later title-sorted page, and a search. The later page starts at offset
40,000 for the reference workload, or at the final full page for smaller
workloads, so reduced runs still exercise real pagination. Each web case must
meet `RUSTY_DLNA_BENCH_WEB_P95_TARGET_MS` (250 ms by default).
Latency threshold failures still write the complete measurements and set
`latency_check_passed` to false before the command exits unsuccessfully.

The scanner uses one online backup to initialize a reusable private stage,
then records changed detail/object/art/caption/playlist/settings keys in
disk-backed journals. A targeted watcher batch must not copy or full-merge the
catalog: local and canonical-parent artwork inventories are built once per
parent, and publication work is proportional to journaled keys. Benchmark
reviews should therefore treat repeated `files.db`-sized I/O or per-media
directory reads during the targeted update as a regression. Capacity samples
must include the private stage, while live-writer latency should remain bounded
during backup, probe, NFO, rebuild, and stage journal cleanup.

## Responsiveness workloads

Set `RUSTY_DLNA_BENCH_SHAPE=flat` to put every physical file in one folder;
`sharded` remains the default. The generator accepts the same optional fourth
argument. Use matching shapes, counts, aliases, workers and request counts for
comparisons. Saved builds can be selected with `RUSTY_DLNA_BENCH_BINARY`,
`RUSTY_DLNA_BENCH_GENERATOR` and `RUSTY_DLNA_BENCH_RECONCILE`; a supplied server
binary skips rebuilding. Reports include the executable hash and shape. Keep
the working directory when measuring restoration separately:

```sh
python3 scripts/large-library-restart-benchmark.py --binary /tmp/rusty-dlna-before --config /tmp/retained-benchmark/benchmark.toml --samples 10 --output /tmp/restart-before.json
```

This uses normal `--database-check` startup to restore the catalog and run SQLite
quick-check without listeners or reconciliation. It measures each process's wall
time, CPU and peak RSS using GNU time. The OS page cache is warm. It is a catalog
restoration measurement, not a second cold scan or a network-ready measurement.

Focused generated-catalog workloads exercise allocation/restoration at
1k/4k/50k in flat and sharded shapes, fixed-size detail-map patches, and SQL and
physical-folder paging at 50k/250k:

```sh
cargo test --locked -p rusty-dlna-scan bundle_e_catalog_scale_measurement -- --ignored --nocapture
cargo test --locked -p rusty-dlna-scan bundle_e_detail_map_patch_measurement -- --ignored --nocapture
cargo test --release --locked -p rusty-dlna-scan browser_query_scale_profile -- --ignored --nocapture
cargo test --release --locked -p rusty-dlna folder_paging_benchmark --lib -- --ignored --nocapture
timeout 180s cargo test --release --locked -p rusty-dlna folder_pages_under_incremental_publication_benchmark --lib -- --ignored --nocapture
```

SQL profiling reports EXPLAIN plans, VM steps, thread CPU and individual wall
samples for first/deep pages and varied searches. Reused population/matching
counts are measured separately from a cold query. Folder profiling clears the
projection cache for cold samples, measures warm pages separately, and runs four
clients with distinct searches while observing catalog-writer acquisition wait
(the synthetic observer does not publish a watcher patch). These
synthetic catalogs isolate query work; they do not replace filesystem scans,
watcher updates, or actual browser measurements. The separate publication
workload applies scanner-journal patches during four-client page traffic at
50k/250k, checks response generation/order, retries 409 responses, and measures
publication duration plus health/status latency. Algorithmic counts and estimated
allocation reductions are separate from process CPU and peak RSS.

The browser workload uses a running isolated server and generated API responses:

```sh
node scripts/library-browser-benchmark.mjs --url=http://127.0.0.1:18201 --cards=10000 --samples=5 --cpu-rate=4 --output=/tmp/library-before.json
node scripts/library-scan-playback-benchmark.mjs --binary=target/release/rusty-dlna --files=4000 --samples=5 --output=/tmp/scan-playback.json
```

Use `--assets=crates/server/web` only when intentionally measuring current assets
against a saved server; omit it for the saved executable's embedded assets. The
Chromium workload records card publication, long tasks, timer scheduling delay,
layout/resize, DOM/heap counts, repeated navigation and playback-tick mutations.
CPU throttling is a repeatable engineering workload, not a physical-device
measurement. Heap diagnosis must release remote DOM handles before concluding
that navigation retains detached cards.

The scan/playback runner creates disposable H.264/AAC media and a seeded catalog,
then admits generated scan files. Each trial starts a fresh server and browser;
it records whether scanning was actually active before and after a presented
frame. Its default 500-ms scan delay is outside the frame-latency window. Server
CPU/RSS excludes browser and helper resources. Every report retains sample counts
and variability; small samples do not establish reliable p95/p99 tails. Keep
reports, traces, heap snapshots and generated libraries outside Git.

## Artwork cleanup regression workload

Artwork cleanup checks references through the `DETAILS(ALBUM_ART)` index,
including in existing databases and reusable scan stages. It scans artwork
rows with indexed reference lookups, so a targeted reconciliation does not do
a full detail-table scan for each artwork row. NULL and zero detail values
remain the no-art sentinel; a shared artwork row survives until its final
reference disappears.

The focused workload uses SQLite VM instruction counts for 512 and 2,048
referenced artwork rows and checks that quadrupling the data does not produce
quadratic growth. It also exercises a one-file update in a reusable stage with
2,048 unrelated artwork references:

```sh
cargo test -p rusty-dlna-scan artwork_cleanup
cargo test -p rusty-dlna-scan targeted_session_keeps_art_heavy_catalog
```

These assertions measure algorithmic work and reference preservation rather
than imposing a machine-specific latency threshold.

## 2026-08-19 reference result

The reference build was measured on Linux 6.8 x86_64 with Rust 1.97.1, an AMD
Ryzen 9 5950X (32 logical CPUs), and 64 GiB RAM:

| Measurement | Result |
| --- | ---: |
| Cold scan wall time | 462.551 s |
| Cold scan process CPU | 495.360 s |
| Resident / peak RSS | 434.383 / 490.125 MiB |
| SQLite files | 115.853 MiB |
| Open file descriptors | 21 |
| Warmup reconcile | 21.480 s, 1 changed |
| Steady unchanged reconcile | 11.590 s, 0 changed |
| Browse p50 / p95 / p99 | 0.397 / 0.411 / 0.448 ms |
| Search p50 / p95 / p99 | 0.413 / 0.425 / 0.434 ms |
| Targeted write-to-Browse update | 719.655 ms |
| Estimated catalog memory | 168.472 MiB |

These figures are a regression baseline for this host, not deployment
guarantees. Storage latency, media complexity, enabled artwork/subtitle work,
and CPU capacity materially affect the result. Compare reports produced on the
same host and settings when evaluating a change.
