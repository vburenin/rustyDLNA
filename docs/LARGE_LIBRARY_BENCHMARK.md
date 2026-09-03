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
sampled at that point. Reconciliation is run twice without loading a second
server catalog: the first result records any one-time database normalization,
and the second is the steady unchanged measurement. Browse and Search each get
10 warmups followed by 200 sequential requests for a 64-object page over
closed local HTTP connections. Update latency starts before an `fsync` of a new
media file and ends when a targeted inotify update is visible through Browse.

The scanner uses one online backup to initialize a reusable private stage,
then records changed detail/object/art/caption/playlist/settings keys in
disk-backed journals. A targeted watcher batch must not copy or full-merge the
catalog: local and canonical-parent artwork inventories are built once per
parent, and publication work is proportional to journaled keys. Benchmark
reviews should therefore treat repeated `files.db`-sized I/O or per-media
directory reads during the targeted update as a regression. Capacity samples
must include the private stage, while live-writer latency should remain bounded
during backup, probe, NFO, rebuild, and stage journal cleanup.

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
