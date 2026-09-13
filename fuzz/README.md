# Parser fuzzing

The separate Cargo workspace uses the pinned nightly and cargo-fuzz versions in
`.github/workflows/ci.yml`. CI builds every target, replays promoted regressions,
then runs each target for ten seconds. The scheduled matrix uses address and leak
sanitizers for ten minutes each. Campaign corpora and crash artifacts are ignored;
reviewed seeds and regressions are tracked.

The `mp4_index` and `profile8_rewrite` targets enable thin `fuzzing` adapters in
the server and transcode crates. They call the production index, playlists and
staging-file rewriter. The feature is disabled in ordinary server builds. No
parser implementation is copied into a target. Each input is capped at 64 KiB;
the rewrite also retains production resource bounds and a two-second deadline.
Files are private and immediately unlinked. The Linux rewrite opens these owned
descriptors through `/proc/self/fd`; corpus and media-library paths are never
opened for writing.

`mp4_index` checks bounded, ordered ranges and positive finite durations, completed
index reuse, and equal histories/playlists when bytes arrive at partial-header
and payload boundaries. Each raw-input case starts with an empty metadata cache:
unlike production's validated completed media, arbitrary bytes cannot trust a
prior completion stamp, and rapid anonymous-inode reuse can alias metadata.
Completed-index reuse is still checked within each case with both descriptors
pinned. Size-zero top-level boxes consume the currently available
bytes and are excluded only from the growing-file equivalence property. A fixed
three-fragment control independently requires 3 seconds, exact byte offsets and
two native segments of 2 and 1 seconds. Truncated fragments and undersized box
headers must be rejected.

`profile8_rewrite` checks file length and byte-for-byte preservation outside the
allowed staging edits, including timing, composition offsets, edit lists, chunk
offsets, and opaque metadata. Every iteration also generates bounded variants of
two independently specified VFR samples. Exact expected bytes require preserved
base-layer VCL, selected replacement RPU, removed enhancement-layer NALs, compacted
sample sizes and `free` configuration boxes. Mismatched VCL and out-of-range chunks
must fail. These structural HEVC controls are parser fixtures, not decoder media
or a substitute for genuine Dolby Vision device validation.

`generate-media-seeds.py` deterministically regenerates the media controls and
expected bytes using Python's standard library. The promoted media regressions
encode existing production parser rejection contracts; they are not claimed to
be newly discovered sanitizer crashes. When a campaign finds a new crash, use
`scripts/promote-fuzz-regression.sh` to minimize and retain it and add a semantic
regression at the owning module.

Run a bounded campaign with a disposable evolving corpus, for example:

```sh
mkdir -p /tmp/mp4-index-corpus
cp fuzz/seeds/mp4_index/* fuzz/regressions/mp4_index/* /tmp/mp4-index-corpus/
cargo +nightly-2026-08-01 fuzz run mp4_index /tmp/mp4-index-corpus -- -max_total_time=60 -max_len=65536 -timeout=5
cargo +nightly-2026-08-01 fuzz run profile8_rewrite fuzz/regressions/profile8_rewrite/mismatched-base-layer.bin -- -runs=1 -timeout=5
```
