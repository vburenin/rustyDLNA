# Plan: project quality and streaming performance

- Status: bundles A (R01–R05), B (P01–P04/P11), and C structural fixes
  (P05/P06/P10) implemented. P07 has measured opt-in resource and pacing
  prototypes; production scheduling/threading/pacing defaults remain unchanged.
  Bundle D implements P09 source-timing repair, bounded stage diagnostics and
  measured final-mux growing delivery. P08 device residency and the larger P09
  raw streaming pipeline remain disabled after their correctness experiments.
  Measurement and verification limits are recorded below; other findings retain
  their individual implementation status. Bundle A's earlier parallel-browser failures and unresolved
  FFmpeg 6 helper crashes remain part of the verification record.
- Baseline: `def6a7e` (`Treat native playback quality as a ceiling`), initially clean worktree.
- Scope: all eight Rust crates, embedded browser, operator tools, deployment,
  CI/release, fixtures, fuzzing, benchmarks, and authoritative documentation.
- Method: primary review plus three subagent reviews of streaming/helper execution,
  scanner/catalog/protocols, and browser/API behavior; focused local reproductions
  and the configured quality gates.
- This is the local plan file explicitly requested by the user. Finding evidence
  and source locations below describe the original review baseline; individual
  implementation notes record the resulting behavior and
  verification limits.

## Goal

Make playback start and seek sooner, sustain more simultaneous streams with bounded
CPU/GPU/memory/disk use, and improve correctness, confinement, operability, and
maintainability. Preserve original video/audio whenever compatible. Preserve HDR,
bit depth, resolution, timestamp alignment, track selection, and renderer contracts.

"Maximum performance" needs separate measurements for time to first displayed
frame, seek-to-frame latency, sustained throughput, and resource cost. Optimizing
one can worsen another. No hardware-specific speedup or new quality default is
promised without representative measurements.

## Non-goals

- The original review was read-only. The subsequent authorized implementation
  covers bundle A, its regressions and documentation, including the user's request
  to use current stable Rust. The user subsequently authorized committing and
  pushing bundle A. Bundle B implementation, regression tests, benchmarks and
  documentation are authorized; committing, pushing and deployment are not.
  Bundle C focused implementation, regressions, measurements and documentation
  are also authorized. The subsequent handoff instruction authorizes committing
  and pushing bundle C; deployment remains outside scope.
  No live configuration change or modification of an existing media library is
  authorized.
- No blanket reduction of probe limits, encoder quality, HDR preservation, or
  security checks to make benchmark numbers smaller.
- No claim that passing tests certifies every renderer, GPU, malformed file, or
  long-running workload. Unmeasured opportunities are explicitly identified.

## Selection guide

Select a bundle letter or individual finding IDs. The recommended first sequence
is **A**, then **B**; add **E/C01** early for large libraries. **D** is a distinct
experimental investment, not a prerequisite for the simpler performance wins.

| Choice | Result to pursue | Findings | Relative scope / confidence |
| --- | --- | --- | --- |
| **A — Correctness and release blockers** | Close nested-media I/O escape, reject incomplete reusable output, recover stalled MSE, preserve job cancellation, restore release consistency | R01–R05 | Several S–L fixes; reproduced core defects |
| **B — Faster playback startup and nearby seeks** | Measure full startup, remove repeated cache scans and full-library barriers, reuse buffered seeks, prioritize media traffic | P01–P04, P11 | M–L; strongest immediate performance evidence |
| **C — Sustained streaming and long-title efficiency** | Reduce read/index/playlist overhead, schedule by resource cost, reuse successful fallback output | P05–P07, P10 | M–L; structural costs established, whole-system gains need benchmarks |
| **D — Advanced GPU and Dolby Vision optimization** | Improve decode/filter/encode transfers and reduce Profile-8 whole-file stages | P08–P09 | L–XL; hardware/prototype validation required |
| **E — Large-library responsiveness** | Remove quadratic work, fix search parity, bound queries, improve paging/memory/playlist scans and browser rendering | C01–C06, B03 | Start with C01/C02/C06; larger redesigns are conditional |
| **F — Captions, accessibility and recovery polish** | Fix subtitle timing/focus, PiP error state, and sustained-playback recovery semantics | B01–B02 | Mostly S–M; several reproduced bugs |
| **G — Safer library-maintenance tools** | Bound conversion jobs, isolate intermediates, preserve permissions and recover interrupted intake | O01–O03 | S–M; permission issue reproduced, resilience gaps traced |
| **H — Validation and maintainability** | Reproduce parallel browser failures, add performance/media coverage, improve dependency and ownership checks | Q01–Q03 | Incremental; avoid a prerequisite broad refactor |

The core streaming recommendation is to remove redundant work before changing
codec quality. Cache lock/scanning costs and the linked-library dependency have
direct evidence. GPU residency, thread budgets, playback-aware pacing, zero-copy
I/O and pipelined Dolby Vision need measured go/no-go decisions.

## Coverage and current strengths

| Area reviewed | Evidence inspected / existing protections | Main proposed work |
| --- | --- | --- |
| `helper` | Fair bounded admission, process groups, null stdin, bounded capture, cancellation/deadline/reaping tests | Preserve generic ownership; P07; apply equivalent controls to O01 |
| `transcode` | Policy/arguments, independent video/audio negotiation, source/tool/cache identities, P8 stages, HDR/GPU fallback and timing fixtures | R02, P07–P10 |
| Server delivery/cache | Admission, registry/leases, pinned growing output, atomic cache publication, HLS index, image caches, status/metrics | R02/R04, P01/P02/P05/P06 |
| Scanner/database | Rooted I/O, probe/admission, NFO/artwork/captions/playlists, aliases, transactional staging/recovery, watcher reconciliation, SQL/load paths | R01, C01–C06 |
| Protocol/HTTP/SOAP | Shared constants, object IDs, framing/ranges/persistence, bounded XML/search/sort, escaping, renderer-specific tests | Preserve contracts; C02/C03 parity/budgets; no new wire defect established |
| SSDP/GENA | Interface/sender handling, bounded reply/work queues, dedup/rate limits, subscription sequence/coalescing, absolute callback deadlines | Existing isolated E2E passed; retain fuzz/privileged validation for relevant changes |
| Browser/UI/API | Every production JS module, controls/style/HTML, source ownership, capability cache, recovery/seek, captions, paging, preview queues, three Playwright specs | R03/R04, P03/P04/P11, B01–B03 |
| Configuration/operations | Strict key/range validation, pure preflight, effective configuration, health, storage budgets, shutdown, systemd and gateway routes | R05, P01, Q01/Q02; no new configuration schema requested |
| Operator tools | Intake/rollback, profile conversion, provider/cache/NFO/artwork, generated views, previews, audits and tests | O01–O03 |
| Delivery/quality tooling | Docker/Compose/systemd, pinned actions/images/locks, release promotion, dependency policies, CI, fuzz seeds, fixtures, coverage/soak/scale scripts | R05, Q01–Q03 |

Useful established design includes original-first playback, bounded source sampling
(three 64 KiB samples rather than full-movie hashing), reused active-generation
source/tool preparation, stable rooted output descriptors, independently negotiated
stream copy, AAC normalization and HEVC tags, staged scanner publication outside
the live writer, nanosecond source timestamps, monotonic browser session ownership,
portable decoder recovery, focused controls staying visible, 44px targets, keyboard
shortcut scoping, reduced motion, and substantial multi-browser behavior tests.

This is a whole-project review with detailed critical-path inspection and scoped
experiments, not a formal proof or a claim that every production combination ran.

## Current playback preparation path

| Path | Existing work before/during playback | Relevant cost |
| --- | --- | --- |
| Original | Confined descriptor, media response/ranges; no FFmpeg job | Preserve this fast route; profile delivery only if P01 shows a bottleneck |
| New compatible generation | Source open/sample, cached-or-new tool fingerprint, plan/key, cache maintenance, helper/title admission, FFmpeg, growth observation, complete fragment/index, browser decode | P01/P02 expose/remove hidden setup costs; R03 prevents silent client hangs |
| Active generation attachment | Reuses immutable source/spec/tool identity and skips new-job scan work | Registry contention can still delay it; avoid duplicating checks per segment |
| Warm completed output | Identity/stamp check and pinned open; **no repeated FFprobe on cache hit** | P02 maintenance and P06 rebuilding indexes remain |
| Compatible seek | MSE reuses buffered media when timeline, decoder prerequisites and source semantics match; other seeks restart | P04 avoids source replacement for eligible buffered seeks |
| Encoded indexed output | One-second forced IDRs, 30-second initial burst then 1x pacing | Preserve known startup/timing fixes; measure P07 rate/concurrency behavior |
| P7 → P8 remap | Private whole-file conversion with source-timing repair, then guarded growing final mux and validated publication | P09 reduces final-mux delivery delay; earlier whole-file stages remain |

## Bundle A implementation verification

- Final `./scripts/agent-verify.sh`: passed on Rust 1.98.1. The gate includes
  120 Python tests, 72 web unit tests, formatting, Clippy with warnings denied,
  workspace tests, Rust documentation checks, fixture checksums, CLI checks,
  eight explicitly enabled socket E2E tests and test-port isolation.
- `scripts/release-contract.sh v0.1.0`: passed without publishing an image.
  The exact Compose compiler image's full compiler assertion also passed.
- Scanner coverage: FFmpeg/libav 6.1.1, 7.1.5 and 8.0.1 each reported 244 passed
  and one intentionally ignored 50,000-row benchmark. The FFmpeg 7 container
  additionally skipped the attached-JPEG fixture test internally because its
  fixture generator was unavailable. Host FFmpeg 6 has that generator.
- Final publication coverage: all 90 host remux tests passed; FFmpeg 8 passed
  all nine publication/validation tests and seven cache tests. Genuine
  Profile-7→Profile-8 conversion passed on FFmpeg 8 with `dovi_tool 2.3.3`;
  the host lacks `dovi_tool`, so its corresponding test returned early.
- First full Playwright run, default 16 workers: **failed**, 654 passed,
  114 skipped and eight failures across 776 cases. The failures were two
  library-scale layouts, five Firefox initial-library loads and one WebKit
  held-frame seek. Three FFmpeg 6 helper crashes were also logged; standalone
  comparisons with both old and new input forms did not reproduce them, so
  their cause remains unknown. This run also exposed the unequal-track-duration
  publication regression, which was corrected and covered on FFmpeg 6 and 8.
- Final complete Playwright matrix with four workers: **passed**, 662 passed,
  114 intentional platform/API skips and zero failures across all 776 cases in
  five minutes. This is a complete Chromium/Firefox/WebKit/mobile-Chromium run,
  not an isolated retry; repository worker policy is unchanged. Two host FFmpeg 6
  Dolby Vision helper SIGSEGVs were logged despite passing browser assertions.
  Their cause remains unresolved; the matrix pass does not establish crash-free
  helper execution, and the final tree was not rerun at the default 16 workers.
  Both helpers reported all 259 frames and final encoder statistics before
  crashing, without preceding cancellation logs for those producer instances.
  No matching core/backtrace is available; local `gdb` and `coredumpctl` are absent.
- `cargo-audit`, `cargo-deny` and `cargo-machete` are unavailable locally.
  Standalone JavaScript/Python lint/type checking remains unconfigured. Dedicated
  GPU/device, ARM64, privileged network-namespace, full release-image smoke,
  sustained soak, fuzz and large-library benchmark runs were not performed.
- Tracked fixtures, lockfiles and live configuration remain unchanged. Validation
  performed no deployments or user-media writes; source commit/push is authorized
  for the subsequent handoff.

## Bundle B measurements and verification

The bundle B baseline was the clean bundle A commit `83012dc`, with a saved
unchanged executable before performance edits. Baseline web units passed 72/72
and remux tests 90/90. Its complete four-worker browser run had 661 passes,
114 intentional skips and one Firefox deep-link enrichment failure. That failure
was retained for investigation and the unchanged assertion is in the final matrix.
It recurred in WebKit after the rate correction. A deterministic held-response
test reproduced the cause: same-title source replacement aborted enrichment and
left its loading state stuck. Enrichment now belongs to the selected title;
source changes preserve it, selection/stop cancel it, and catalog changes reject
stale completion. The original test now uses matching metadata/media and a
completed mocked producer while retaining real successful server enrichment.
The final focused retry/navigation suite passed 84/84 cases across four browsers.
Two additional regressions reproduced a server catalog change between the plain
linked item and its enrichment while the browser still held the earlier library
generation. Linked enrichment now carries that generation precondition and
rejects mismatched responses before committing playback.

Final `./scripts/agent-verify.sh` passed on Rust 1.98.1: 121 Python tests,
100 web unit tests, workspace tests, formatting, Clippy with warnings denied,
Rust documentation checks, fixture checksums, CLI checks, eight enabled socket
E2E tests and port isolation. The server suite passed 313 tests; its cache-scale
benchmark is intentionally ignored by the ordinary gate and was run separately.
The scanner's unrelated 50k-row benchmark remains ignored. The final complete
four-worker Playwright matrix passed 738 tests with 146 explicit platform/API
skips and zero failures across 884 cases in 6.0 minutes. The default 16-worker
matrix was not run. An earlier socket gate failed because a concurrently running
browser server shared test port 18201; sequential execution resolved that test
collision, and all eight final socket cases passed.

The first integrated B matrix had 688 passes, 126 skips and six failures.
First-page capability publication caused duplicate loading announcements, fixed
without weakening the announcement test. WebKit recovery was reproduced under
repetition: a queued pause event arrived while the element was playing and
incorrectly changed saved intent. Checking the element's current pause state
fixed it; the regression then passed 16/16 repetitions. Page-50 startup and frame
presentation passed before release, but WebKit exceeded the separate 7.5-second
10k-card rendering assertion. Its trace eventually showed all 10k cards, hidden
loading state and the singleton queue. Only that post-release assertion now has
a 30-second settling budget; full-list rendering remains expensive. Review also
added regressions for delayed first-frame callbacks, overlapping preview work,
predecode JPEG bounds and a library retry changing the pending linked generation.

One FFmpeg 6 Dolby Vision helper SIGSEGV was logged in the first integrated
matrix. Bundle A had previously recorded similar crashes, but this B baseline
run logged none. The final passing matrix logged three such crashes. The cause
remains unresolved. The failing input is the checked-in, checksum-matched
Profile 7/TrueHD fixture; an inspected attempt crashed during software conversion
to H.264 SDR/AAC after emitting 259 frames. The fixture contains real BL/EL/RPU
data, and this is not an expected invalid-input rejection. Passing assertions
cannot certify crash-free helper execution. No FFmpeg output arguments, stream-copy decisions,
encoder quality settings or output-cache identity were changed by B.
The final browser log is `/tmp/rustydlna-bundle-b-playwright-final.log`; the
finished-source gate log is `/tmp/rustydlna-bundle-b-agent-verify-complete.log`.

The final matched playback comparison used ten independent trials per workload:
generated 40-second 1280×720/24-fps SDR H.264/AAC, reordered copied video,
one Chromium viewer at 1×, debug binaries, Ryzen 9 5950X, FFmpeg 6.1.1,
and ext4 with the OS page cache warmed by fixture generation. Cold means empty
derived output and a fresh server/tool cache; warm requires validated completed
output and no new media helper. Binary hashes, output recipes, dimensions,
color metadata and bounded decoded-frame hashes are recorded and compared.

| Workload | Median ms, before → after | Observed p95 ms, before → after | Sample SD ms, before → after |
| --- | ---: | ---: | ---: |
| Original selection | 36.10 → 34.10 | 44.72 → 45.54 | 6.51 → 7.22 |
| Cold Compatible selection | 169.65 → 178.35 | 209.90 → 220.48 | 19.66 → 25.16 |
| Warm Compatible selection | 54.75 → 52.75 | 60.99 → 63.39 | 6.51 → 5.09 |
| Nearby playing seek | 519.20 → 69.95 | 528.42 → 78.69 | 9.85 → 6.95 |
| Nearby paused seek | 525.05 → 75.60 | 535.48 → 79.56 | 7.55 → 4.77 |
| Restarted seek | 507.40 → 544.40 | 564.75 → 558.74 | 30.64 → 28.94 |

All ten final nearby trials of each intent reused the source; all ten far seeks
restarted. Cold startup and restarted-seek medians were slower in this run; there
is no general startup-speedup claim. Ten trials do not establish reliable p95/p99
tails. The earlier after run reused only three of ten nearby seeks per intent;
its retained report exposed the initial-idle and busy-buffer races fixed above.
The primary pair is `/tmp/rustydlna-bundle-b-playback-before.json` and
`/tmp/rustydlna-bundle-b-playback-rate-verified.json`. The latter verifies actual
speed at presentation, seek completion and sustained-window boundaries. The saved
baseline did not capture first-frame speed; its requested 1× and sustained 1×
observations are the available rate evidence. Copy producers finished too
quickly for observable active attachment or cancellation; those cases are marked
unavailable for this recipe, not counted as successful cancellation samples.
These measurements precede the final enrichment-ownership fix; their generated
fixtures already have complete metadata and do not enter that retry path.

In matching sampled cold-start resource windows, median server/helper CPU was
0.180→0.185 seconds (sample SD 0.055→0.056 seconds), median sampled peak RSS
45.35→45.88 MB, physical reads zero and writes 37.03 MB for both. Browser CPU
was 0.510→0.495 seconds; median sampled peak RSS was 689.99→691.68 MB and writes
36.99→38.71 MB. Nearby-seek browser CPU was 0.395→0.190 seconds (SD
0.042→0.018), sampled peak RSS 755.11→719.90 MB and writes 35.61→14.32 MB.
These are whole-window observations, not hard memory bounds or isolated server
efficiency claims. Process sampling can miss short-lived helpers and peaks;
summed RSS includes shared pages. Its cumulative cold-window sampling overhead
was about 333→328 ms, recorded separately from playback latency. Two-second
cold-playback samples sustained approximately 1.000× with zero reported dropped
frames on both binaries. This does not establish prolonged-load behavior.

The matched one-trial CPU recipe subset exercised video copy/audio encode,
video encode/audio copy and both encode, with actual attachment to a live
producer, output validation and cancellation/reaping. Cancellation before→after
was respectively 407→319, 356→356 and 358→347 ms. These single observations
provide functional coverage, not cancellation-performance conclusions.
Those recipe measurements preceded the final saved-rate correction; the final
binary has separate 1×/2× measurements and the complete verification gate.

The requested 2× viewer trials initially advanced at 1× on both binaries. They
are excluded from successful 2× coverage. A real-browser regression reproduced
native source loading resetting the actual rate while the saved preference and
selector still displayed 2×. Source loads now preserve the selected native
default rate, and the harness records/asserts actual rate. A bounded unchanged-
baseline reproduction fails that assertion and is retained. After-only 720p
checks with two viewers/30 fps and four viewers/60 fps preserved 2× at startup
and across playing/paused/restarted seeks; all six and twelve sustained windows
respectively passed the progression screen (1.9925–2.0046× and 1.9957–2.0054×).
Each is one trial with correlated viewers. Dropped frames ranged from 0–15 and
122–128 per two-second window respectively. This establishes rate and seek
functionality, not smooth 120-fps presentation or comparable 2× baseline
performance. The report comparator now rejects recorded rate mismatches or failed
sustained-progression screens; endpoint-limited windows remain inconclusive.

Cache reports use generated entries and the actual maintenance API; the linked
browser reports use generated decodable media and controlled page delay. Reports,
temporary libraries and logs stay outside Git under `/tmp/rustydlna-*`. Cache
operation counts alone do not support a playback speedup. Small-sample playback
p95/p99 remain descriptive, and CPU/memory/I/O claims require matching sampling
windows and conditions. The saved bundle A binary exposes its existing server
phases; the new detailed source/tool/admission/helper breakdown is available only
after instrumentation. Those new stages are not a before/after substage comparison.
The existing selected findings below retain their
original review evidence after their implementation notes.

Dedicated GPU/HDR/Dolby Vision presentation, P7/P8 conversion performance,
physical devices, cold storage/page-cache drops, concurrent scanner/artwork
playback benchmarks and prolonged resource pressure are not established by the
CPU subset. Concurrent artwork/cache ownership is covered behaviorally. The
generated CPU clips end audio before video: an exploratory audio-longest clip
exposed the existing selected-video-track tail indexing limitation, which is
reported rather than counted as a successful warm-MSE trial. Privileged network
namespace, Docker release smoke, ARM64, soak and fuzz campaigns were not run for
this scoped change. Local `cargo-audit`, `cargo-deny` and `cargo-machete` remain
unavailable; standalone JavaScript/Python lint/type checking remains unconfigured.

## Bundle C implementation and measurements

The current baseline is clean commit `cc7724c`, with Rust 1.98.1 and a saved
unchanged debug executable (`/tmp/rustydlna-bundle-c-before`, SHA-256
`4404eeb9e083fc5ef5d3b18ccb80491ae913e0dacd63c13f138da9cd320265d1`).
Existing bundle A/B/E contracts remain in place. No production codec, encoder
quality, HDR policy, thread count, pacing argument or title/helper ceiling changed.
[ADR 0004](docs/adr/0004-streaming-output-and-index-identity.md) records descriptor,
index and effective-output identity decisions.

P05 uses five rotated samples per variant, four loopback readers transferring
8 GiB while a separate index worker reads 512 MiB of fixed sparse metadata.
The same generated 64 MiB fixture, warm ext4 page cache, Ryzen 9 5950X and
Rust 1.98.1 release build apply to every variant. This isolates primitives;
it excludes HTTP guards, real demux/decode, storage-cache misses and renderer work.

| Read strategy | Median CPU seconds/GiB (sample SD) | Median GiB/s (sample SD) | Median voluntary switches/GiB |
| --- | ---: | ---: | ---: |
| Legacy 64 KiB | 1.258 (0.075) | 3.281 (0.180) | 80,347 |
| Positioned 64 KiB | 0.820 (0.054) | 6.669 (0.445) | 29,115 |
| Positioned 256 KiB | 0.543 (0.032) | 9.410 (0.606) | 12,897 |
| Positioned 1 MiB | 0.517 (0.209) | 9.622 (2.591) | 7,817 |

256 KiB is selected: 1 MiB adds only 2.3% median throughput with much higher
variance and four times the per-socket buffer. Involuntary switches increase
from 3.875 to 12.875/GiB. First-write medians are 422/368/370/396 microseconds
(20 correlated socket observations per variant), not displayed-frame latency.
Five quiet slow-client cancellation runs have median 5.876 ms, SD 0.082 ms,
range 5.716–5.910 ms. Process peak RSS is cumulative across variants and cannot
establish comparative memory use; the one-buffer/socket bound is structural.
Reports: `/tmp/rustydlna-bundle-c-p05-summary.json`,
`/tmp/rustydlna-bundle-c-p05-sustained.log`, and
`/tmp/rustydlna-bundle-c-p05-cancel.log`.

P06 compares optimized original/current parsers with 18 synthetic samples per
revision/shape, in two alternating process runs. Values below are the range of
the two nine-sample medians, not confidence intervals. URIs are 200 characters.

| Shape | Warm index ms, before → after | Render ms, before → after | Manifest bytes | Bytes/minute at assumed 1 Hz |
| --- | ---: | ---: | ---: | ---: |
| 10 minutes / 600 | 1.907–2.017 → 0.003647–0.003687 | 0.206–0.208 → 0.148 | 150,293 | 9,017,580 |
| 2 hours / 7,200 | 22.907–23.838 → 0.002555–0.002665 | 1.799–1.857 → 1.065–1.093 | 1,806,244 | 108,374,640 |
| 8 hours / 28,800 | 91.578–95.815 → 0.003526–0.003557 | 7.481–7.809 → 4.370–4.421 | 7,244,145 | 434,648,700 |

Manifest bytes are identical before/after. Native EVENT network cost is not
reduced. Eight-hour retained metadata is 2,359,660 → 2,100,028 bytes. Ten-minute
cold parse median regresses 6–13% (1.902–2.010 → 2.138–2.146 ms); eight-hour
cold parsing improves 91.631–95.666 → 63.103–64.172 ms, including P05 positional
reads. There is no blanket cold-index speedup claim. All samples/minima/maxima
are in `/tmp/rustydlna-p06-comparison-final.log`; its reproduction harness is
`/tmp/rustydlna-p06-comparison.py`.

Separate decodable fixtures contain real 32×32/1-fps H.264 video only, for
10 minutes, two hours and eight hours. Full decoding and completed-output
validation passed. Nine after-only debug samples had warm medians
0.005951/0.004719/0.007614 ms; render medians 0.418/2.960/11.611 ms. Four
64 KiB positioned readers during formatting observed p95 read latency
0.007134/0.006853/0.006523 ms (1,455/20,389/94,925 correlated reads). These are
not a matched before/after segment-latency comparison or representative UHD
media. Report: `/tmp/rustydlna-p06-media-benchmark.log`. Native Safari, mobile
hardware and variable-GOP native recovery are not certified by these tests.

P07's opt-in mixed experiment uses five rotating-order trials per arm, the same
40-second 640×360/24-fps SDR H.264/AAC fixture and output profiles, eight allowed
logical CPUs (affinity 0–7), a four-helper ceiling, and the RTX 3050 8 GiB with
driver 555.42.06/FFmpeg 6.1.1. Only the explicit threading and admission arms
differ; video-only, both-track encode and NVENC remain separate workloads.
This environment does not expose usable cgroup controller files, so a real
cgroup-quota tier was unavailable; affinity limits were exercised.
Every produced file decoded successfully. This is unpaced raw-MP4 throughput,
not browser start/seek performance or new pacing validation.

| Observation, median ms (sample SD) | FIFO, automatic threads | FIFO, bounded pools | Resource admission, bounded pools |
| --- | ---: | ---: | ---: |
| CPU video helper | 749 (14.9) | 1,814 (26.5) | 1,934 (60.1) |
| Both-track helper | 1,354 (28.0) | 2,541 (26.0) | 2,539 (69.5) |
| NVENC helper | 1,634 (65.4) | 1,994 (48.1) | 2,017 (143.2) |
| Copy queue | 81 (9.1) | 61 (0) | 2,058 (60.6) |
| Audio queue | 61 (0) | 61 (0) | 1,935 (60.2) |
| Artwork queue | 122 (8.4) | 101.5 (0) | 2,269 (60.2) |
| Probe queue | 91.5 (8.5) | 71 (0) | 1,996.5 (60.4) |

Probe/artwork values average two jobs within each independent trial. Sampled
CPU-video thread peaks drop from median 35 to 4, and NVENC from 35 to 10;
CPU-video sampled CPU ticks drop 205 → 182 with bounded FIFO pools, while
helper duration more than doubles. Sampling at about 20 ms misses short-lived
work and is a lower bound. Reports retain exact commands and per-helper CPU,
thread, RSS and first-16-KiB observations; GPU engine utilization/VRAM, other
GPUs and AI-upscale execution are not measured. Existing upscale admission is
preserved; its resource model is tested separately.

For promotion, interactive queue/first-output screening budgets are 500/1,000 ms,
background queue 10 seconds, at least 2× unpaced encoded throughput, and the
existing four-helper ceiling. The resource arm fails the interactive budgets;
no class times out, and raw throughput alone is insufficient evidence. We do
not promote either threading or admission changes. A two-hour synthetic demand
model preserves a bounded lead at 2× and stops additional demand when all viewers
pause, while keeping another active viewer/native consumer independent. It does
not control real FFmpeg; fixed-1× lead exhaustion and continued paused production
remain production limitations. SIGINT/SIGTERM tests reap active prototype helpers
and remove temporary output in 214/215 ms; no live configuration is changed.
Reports: `/tmp/rustydlna-bundle-c-p07.tsv`, `-p07.log`, `-p07-summary.json` and
`-p07-signals-final.json` with the same prefix. The initial wrapper pilot failed
because argv[0] was passed twice; it is retained as `-p07-pilot.*` and excluded.
The final signal/cleanup refinements leave the measured helper recipes unchanged.

P10's actual FFmpeg unsupported-encoder regression publishes validated software
H.264/SDR output, then reuses it without another producer attempt. Tests cover
expiry, changed candidate recipes/device identities, invalid output, concurrent
owners, cancellation, status-before-output, pinning below 16 KiB and unpinned
output above it. A retained completed fallback cannot bypass new-owner checks;
existing generations retain their pinned output. This is evidence for the
mechanism, not a claim that every physical GPU failure was reproduced.
Review also corrected a Growing-to-fallback readiness/pin race within the
original deadline and made MSE use the pinned fallback's codecs before init
append. A warm HEVC-request/H.264-output HTTP regression and fresh/cached browser
decode cases cover that boundary without changing the playback generation.
An initial browser run also exposed loss of effective stream facts after the
short fixture ended. Facts now remain bound to the completed item/session,
while a replay or replacement cannot inherit them; active, ended and replay
states are covered explicitly.

The matched P01 integration run uses ten independent trials per recipe, two
concurrent Chromium MSE viewers at verified 2×, 40-second 640×360/24-fps SDR
H.264/AAC fixtures, FFmpeg 6.1.1, debug executables and warm ext4 page cache.
Cold means empty derived output/fresh server; warm requires a validated stamp
and no new producer. All 40 output recipe/probe/decoded-frame-hash comparisons
pass with no configuration mismatch. Startup observations from the two viewers
are correlated; seek/cancellation have ten observations per workload.

| Recipe | Cold frame ms (SD), before → after | Warm frame ms (SD), before → after | Restart frame ms (SD), before → after | Cancellation ms (SD), before → after |
| --- | ---: | ---: | ---: | ---: |
| Copy | 166.95 (7.57) → 168.00 (19.71) | 41.85 (6.04) → 45.40 (6.30) | 487.80 (7.39) → 491.85 (28.15) | unavailable |
| Audio encode | 207.60 (11.45) → 208.50 (38.62) | 44.45 (5.95) → 48.05 (10.85) | 535.80 (5.27) → 536.35 (6.80) | 348.12 (34.97) → 352.40 (101.22) |
| Video encode | 215.50 (5.18) → 210.25 (13.37) | 39.40 (5.39) → 45.15 (5.20) | 586.75 (28.15) → 584.25 (24.32) | 326.99 (37.26) → 345.47 (23.22) |
| Both encode | 266.90 (12.49) → 261.75 (9.21) | 42.95 (5.97) → 41.85 (6.41) | 590.40 (5.38) → 594.15 (7.73) | 316.09 (47.96) → 357.45 (40.65) |

There is no general startup/seek improvement. Warm medians regress for three
recipes, and cancellation medians regress for all encoded recipes. Copy
producers finish before active attachment/cancellation can be measured; those
20 unavailable cases are not passes. The audio cold observed p95 rises
219.77 → 328.52 ms (one trial affects both viewers); cancellation observed p95
rises 386.60 → 551.36 ms. Both trigger P01's repeat recommendation. These small
samples do not establish reliable tail distributions.

Separate three-second post-validation playback windows all progress near 2×
(observed media/wall range 1.992–2.003). Producers have already completed in
these windows; zero physical reads/writes and median sampled server CPU
0.020–0.030 seconds cannot establish production pacing behavior or CPU/GiB.
Median server RSS increases by 0.66–1.70 MB across recipes. Per-trial startup
and sustained CPU/RSS/I/O remain separate in the report, with concurrent viewers
counted once. The baseline was collected while agents also edited and sometimes
compiled/tested; the final measurement window excluded other heavy checks.
Therefore integration differences are observational, not isolated causal gains.
The rotated P05/P06 primitive benchmarks provide the stronger structural evidence.

Reports are `/tmp/rustydlna-bundle-c-playback-before.json`,
`/tmp/rustydlna-bundle-c-playback-after.json` and
`/tmp/rustydlna-bundle-c-playback-summary.json`. The measured after executable
is `/tmp/rustydlna-bundle-c-after`, SHA-256
`2b149d9d1d5d17f0583cb8eff6d79351282bb977a3cfa7862e824f72d788e516`.
This pair precedes the final readiness/fallback-MSE handshake corrections;
the successful primary H.264 benchmark recipes do not exercise fallback.

A separate matched audio-only repeat uses ten new independent trials per
revision with the same fixture/codec/quality/storage/concurrency/rate options,
run sequentially after other heavy checks stopped. All ten output validations
pass; P01 reports comparable configurations and no repeat flags. Cold-frame
median is 214.35 → 213.10 ms (SD 7.89 → 10.35), observed p95
225.31 → 223.38 ms. Cancellation median is 325.26 → 308.63 ms
(SD 40.30 → 37.28), observed p95 342.58 → 339.67 ms. Warm median
still rises 41.55 → 44.10 ms; paused-nearby seek rises 26.55 → 32.35 ms.
The original slower observations remain above; the repeat does not establish
reliable tail performance or a general speedup. Reports:
`/tmp/rustydlna-bundle-c-audio-repeat-before.json` and
`/tmp/rustydlna-bundle-c-audio-repeat-after.json`. This repeat uses the final
production executable `/tmp/rustydlna-bundle-c-final`, SHA-256
`f5b231490186bd9532fe662e9b70938b93602b151ed64cbc9ba6d671247908cd`.

Final `./scripts/agent-verify.sh` passes on Rust 1.98.1: 121 Python tests,
102 web unit tests, workspace tests (366 server tests), formatting, Clippy with
warnings denied, Rust documentation checks, fixture checksums, CLI checks,
eight enabled socket E2E tests and test-port isolation. The final gate also
covers the last history-append refinement removing an unnecessary invariant
panic; browser matrix behavior is unchanged. Seven P07 example policy tests and
its SIGINT/SIGTERM cleanup checks pass separately. The gate intentionally ignores
six server/four scanner benchmarks; the three bundle C index/delivery benchmarks
were exercised separately, while unrelated cache/library benchmarks were not
rerun. Standalone JS/Python lint/type checking remains unconfigured; local
cargo-audit/deny/machete were not available. Gate log:
`/tmp/rustydlna-bundle-c-agent-verify-handoff.log`.

Bundle C verification: the full four-worker Playwright matrix passes 758 tests,
with 150 explicit platform/API skips and zero failures across 908 cases (6.2
minutes). This is Chromium/Firefox/WebKit/mobile Chromium, not native Safari or
physical-device certification. The default 16-worker matrix was not run. No
FFmpeg SIGSEGV was logged in this run; earlier A/B crashes remain unresolved.
Focused fresh/cached cross-codec tests decode real H.264 frames, with HEVC
capability mocked, on desktop/mobile Chromium. Effective-facts tests pass all
four projects through paused playback, natural end and keyboard Replay.

`scripts/helper-load.sh` passes 24 requests: sampled peaks 59,432 KiB RSS,
64 threads and 57 FDs; cache 853,923 bytes; longest response 0.352 seconds.
The sampler observed at most one process and can miss short-lived helpers;
this is the existing bounded-load regression, not a new process-cost estimate.
The fixed-tree 180-second soak passes 18 socket-suite cycles over 184 seconds,
with sampled peaks 57,612 KiB RSS, nine threads/nine FDs, two processes,
962,517 database bytes and 10,531 cache bytes, with no temporary-tree leaks.
It is a restart/socket soak, not 24-hour persistent streaming evidence. The
full browser matrix and the first soak cycles overlapped on separate ports;
performance measurements did not overlap these checks. An initial soak was
invalidated by a concurrent regression-test edit and is retained separately.

Logs: `/tmp/rustydlna-bundle-c-playwright.log`,
`/tmp/rustydlna-bundle-c-helper-load.log`,
`/tmp/rustydlna-bundle-c-soak-final.log` and `-soak-final.tsv`.
Native Safari, physical mobile tiers, ARM64, GPU engine/VRAM instrumentation,
other GPU families, AI-upscale execution, cgroup quota enforcement, cold storage,
24-hour soak, privileged namespace tests, Docker release smoke and fuzz campaigns
remain unavailable or outside these exercised surfaces. No production defaults,
fixtures, lockfiles or live configuration were changed. Commit and push are
authorized for this bundle C handoff; deployment remains outside scope.

## Bundle D verification

The final canonical `./scripts/agent-verify.sh` run passes on Rust 1.98.1 with
temporary `DOVI_TOOL` pointing to 2.3.3. It includes 121 Python tests, 102 web
unit tests, formatting, Clippy with warnings denied, workspace tests, Rust
documentation, fixture checksums, CLI maintenance, eight explicitly enabled
socket tests and port isolation. Ten opt-in Rust scale/streaming benchmarks
remain intentionally ignored by that gate. Its first run failed one unmodified
watcher test (`real_inotify_tracks_a_hard_link_writer_unlinked_before_close`)
with a catalog timeout; that test passed independently and in the complete
rerun. No watcher code or timeout was changed. Logs remain outside Git as
`/tmp/rustydlna-bundle-d-agent-verify.log`, `-agent-verify-rerun.log` and
`-agent-verify-final.log` (after the final benchmark-reporting corrections).

Focused regressions exercise genuine MEL CFR/VFR with FFmpeg 6.1.1 and 8.0.1
and dovi_tool 2.3.3, without tool-availability skips. They compare decoded
video/audio, source packet timing, selected audio offsets, RPU bytes, signaling
and seeks. Parser, every-stage cancellation/failure/pressure, real ENOSPC,
pre-exposure readiness, pinned failure, final validation and atomic publication
tests pass. GPU and actual HTTP measurements, sample counts, variability,
correctness failures and deliberate no-go decisions are recorded in P08/P09.

The full Playwright matrix with default 16 workers completed 908 cases:
749 passed, 150 skipped and nine failed. All nine failures passed unchanged
with `--last-failed --workers=1`. Four Firefox traces spent 20.6–27.9 seconds
of their 30-second budgets loading the initial page; two others stalled on
initial library fetches. Three WebKit first-frame failures use intercepted
direct H.264 and bypass Profile-8 entirely. This supports load sensitivity,
but does not establish a passing parallel matrix. The 150 existing skips cover
project-specific codecs, MSE/native HLS, fullscreen and mobile layouts; they
are not passes. Browser assets, tests and worker settings were not changed.
Logs are `/tmp/rustydlna-bundle-d-playwright.log` and `-playwright-rerun.log`;
original traces are retained in `-playwright-first-artifacts.tar.gz`.

`scripts/helper-load.sh` passes 24 requests within its existing limits:
sampled peaks 59,280 KiB RSS, 64 threads, 40 FDs and one process; cache
890,521 bytes; longest response 0.584 seconds. Short helpers can escape
sampling, so the process peak is not a claim that no helper ran. Log:
`/tmp/rustydlna-bundle-d-helper-load.log`.

A separate one-trial Chromium MSE check uses a generated 60-second 720p24
MPEG-2 SDR/AAC source, existing Balanced H.264 NVENC/AAC-copy output and
source-bounded `full_hd`→`data_saver` selection. Both 25-second windows pass
at 1× (600/599 presented frames, zero reported drops, no endpoint reached).
Warm stamped reuse, live attachment, buffered/paused/restart seeks and actual
helper cancellation (279 ms) pass. This validates the benchmark's requested/
effective-quality reporting and bounded playback; one trial establishes no
performance gain. An initial 30-second-window trial reached the clip endpoint
and is retained as inconclusive. Reports: `/tmp/rustydlna-bundle-d-sustained.json`
and `-sustained-final.json`, with matching logs.

The fixed-tree 120-second soak passes 12 socket/restart cycles over 121 seconds:
sampled peaks two processes, nine threads, nine FDs, 56,780 KiB RSS,
921,605 database bytes and 10,531 cache bytes; no temporary-tree leaks.
`/tmp/rustydlna-bundle-d-soak.tsv` records the exact source fingerprint before
this verification-note update; production code is unchanged. The soak and
performance measurements ran separately. This is not a 24-hour soak or
feature-length display test.

No physical Dolby Vision display, genuine FEL video, UHD conversion workload,
other GPU family, driver reset, physical VRAM exhaustion, cold-storage run,
privileged network suite, Docker release smoke or fuzz campaign was validated
for this bundle. Unvalidated GPU residency and concurrent raw conversion stay
disabled. Tracked fixtures, lockfiles, user media, live configuration, drivers
and deployment remain unchanged. Commit and push are authorized for this
bundle D handoff; deployment remains outside scope.

## Original review evidence and verification

- `./scripts/agent-verify.sh`: passed. This includes the canonical gate's Python
  and shell checks, fixture checksums, 71 web unit tests, Rust formatting, Clippy
  with warnings denied, workspace tests, rustdoc checks, CLI maintenance checks,
  eight explicitly enabled socket E2E tests, and test-port isolation checks.
- Python suite: 110 passed. One Rust 50,000-row benchmark is intentionally ignored;
  it was not exercised by this gate. Rust documentation checks completed, although
  there are no runnable Rust doctest cases in the current crates.
- `scripts/release-contract.sh v0.1.0`: **failed**, exit 1:
  `Docker build image does not use Rust 1.97.1`.
- Full Playwright matrix: **failed**, 564 passed, 112 skipped, four failures in
  680 scheduled cases. The failing cases are library-scale layout in Chromium and
  mobile Chromium, retry-budget behavior in Firefox, and held-frame seeking in
  WebKit. `npx playwright test --last-failed --workers=1` then passed all four in
  34.1 seconds without source changes. These are timing/concurrency-sensitive
  validation failures with root cause unproven; the full run remains a failure.
- `npm audit --json`: passed with zero reported vulnerabilities for the installed
  lockfile dependency graph. This is the audit service's result, not a guarantee.
- `cargo-audit`, `cargo-deny`, and `cargo-machete` are unavailable locally. Their CI
  configurations and `deny.toml` were inspected; these audits were not run here.
- Full production-media, dedicated GPU, privileged network-namespace, Docker smoke,
  sustained soak, fuzz campaign, and large-library benchmark runs are outside this
  read-only review's executed validation. Their existing scripts/workflows were reviewed.
- JavaScript/Python standalone lint/type checking is unconfigured, as documented
  in `docs/INDEX.md`; syntax/unit coverage is not a substitute for those checks.

Evidence labels used below:

- **Reproduced**: exercised the behavior in this worktree or extracted production
  function, with the scope and limitations stated.
- **Code-backed**: traced the relevant control flow; workload impact is not measured.
- **Experiment**: plausible optimization requiring measurement and a go/no-go decision.

Priority: **P1** = highest proposed priority (correctness, trust, release failure,
or a clear major performance cost); **P2** = next; **P3** = optional follow-up.
Effort is S/M/L/XL relative to this project, including tests: S is a contained fix,
M spans an owner and regressions, L spans multiple subsystems or a benchmark campaign,
and XL is a pipeline/architecture experiment. These are not delivery dates.

## Steps and detailed findings

### R01 — Confine nested media-demuxer I/O and reject unrecognized containers

**Implemented.** An explicit container/demuxer set replaces unknown-to-Matroska
fallbacks. Scanner probing and attached artwork use descriptor AVIO with nested
opens denied before discovery; external media helpers use seekable inherited
`fd:` inputs with no arbitrary `file` or network protocol. Probe revision 7
rechecks older admissions and removes positively identified unsupported manifests
and aliases in staged publication. Actual scanner/helper regressions watch
outside-root sentinel access and ephemeral HTTP listeners, including MOV external
data references. The native adapter is tested with FFmpeg/libav 6.1.1, 7.1.5
(the exact Compose compiler image), and production 8.0.1;
external FFmpeg/FFprobe now require version 6 or newer. See
[the confinement contract](docs/PROTOCOL_CONTRACT.md) and
[the input decision](docs/adr/0003-confined-media-inputs.md).

**P1 · Reproduced through the actual scanner · M/L.** A validated top-level file
descriptor does not confine resources subsequently opened by libav demuxers.
`crates/scan/src/probe.rs:421–450` sets probe byte/time limits and an interrupt
callback, but no demuxer allowlist or nested-I/O rejection. Probing runs before
final admission, including ambiguous-format handling in `crates/scan/src/lib.rs:3934`.
The fallback in `probe.rs:947` maps unrecognized formats to `mkv`.

With `wide_links=false`, a temporary root containing DASH XML named `dash.mp4`
referenced a copied test fixture outside that root. Running the real `--rescan`
command admitted the manifest as `video/mp4`, recording H.264/AAC and the external
fixture's duration. A second manifest referenced an ephemeral localhost HTTP
server; it received `GET /sentinel.ts` during the actual scan. This proves both
outside-root media access and attacker-directed network access from media content
on the tested host. It requires the ability to place or replace media-root content;
this review did not establish an unauthenticated remote upload path or arbitrary
secret disclosure. The reproduction used host FFmpeg/libav 6.1.1; production
FFmpeg 8 must be included in the fix's regression matrix.

1. Define the admitted demuxer set explicitly and return an unsupported result
   for unknown formats. Extension/MIME fallback must not relabel DASH as Matroska.
2. Prevent secondary resource opens before stream discovery. Prefer descriptor
   AVIO with an explicit nested-open rejection policy; allow only deliberately
   provided descriptors when a supported format genuinely needs multiple inputs.
3. Apply the same boundary to scanner artwork/thumbnail decoding and external
   FFmpeg/FFprobe invocations, auditing each input rather than only `-i`'s pathname.
   A protocol allowlist alone containing `file` does not prevent arbitrary local
   file references. Retain deadlines/cancellation and top-level rooted descriptors.
4. Add temporary outside-root sentinels and ephemeral HTTP listeners to regression
   tests for disguised manifests, nested resources, and legitimate admitted formats.

**Accept when:** neither local sentinel nor HTTP server is accessed; unsupported
manifest inputs are rejected before publication; all supported fixture/container,
non-UTF-8, hardlink, symlink-retarget, and cancellation cases remain correct.
The tests must exercise the actual scanner and helper paths on supported libav
versions. FFmpeg documents protocol restrictions separately from nested protocols;
the project's restriction must also cover local nested paths.
([FFmpeg protocol options](https://ffmpeg.org/ffmpeg-protocols.html#Protocol-Options))

### R02 — Validate completed fragmented MP4 beyond successful probing

**Implemented.** Completed publication now validates bounded MP4 initialization,
all requested tracks/codecs, fragment boundaries, sample extents and decode
timelines before Complete or a reusable stamp. Verification and publication share
the remaining original deadline; cancellation and metadata-visible mutations fail
closed. Cache identity and stamps are versioned. Actual publication regressions
cover malformed/truncated/init-only output, missing/wrong tracks, cancellation,
mutation, short media, HDR and mixed copy/encode on FFmpeg 6.1 and 8.0.1. A
60-second fixture validates about 35 KiB of metadata in approximately 1–2 ms.
Completed GET/range access updates stamp recency while keeping validated media
metadata unchanged, preserving reusable-cache hits and stable `If-Range` tags.
Regression coverage also preserves the existing 32-audio-plus-video download
limit, referenced DLNA chapter text, VP9/AV1/MPEG-2 video copies, and DTS audio.
The full browser run exposed legitimate unequal track lengths; coverage compares
the longest track with catalog duration while checking every track's continuity.
Individual source track endings are not stored, so their exact terminal coverage
cannot be proved. Payload decoding and authenticated cache integrity are outside
this structural check. See [validation limits and tolerances](docs/TRANSCODE.md).

**P1 · Reproduced validator gap · M.** The streaming reviewer generated a valid
four-second fragmented MP4, truncated its final `mdat` payload, and ran the exact
production final-validation FFprobe arguments (`remux.rs:1475–1523`). Both files
returned success with empty stderr. The existing `crates/server/src/remux/hls.rs` index parser rejected
the truncated file when finalized. A successful metadata probe is therefore not
sufficient proof that all output bytes are present.

1. Validate final box boundaries, required initialization/tracks, completed
   fragments, sample-data extents and expected timeline coverage before reusable
   cache publication. Extend the existing parser with aggregate work bounds:
   it currently extracts selected-track duration, skips sample sizes/offsets,
   and does not validate all tracks or `tfdt` timelines. Its structural rejection
   of the reproduced truncation is useful but does not prove full media validity.
2. Check requested codecs/tracks and duration against the negotiated plan, allowing
   documented timestamp/seek tolerances. Do not require decoding the entire movie
   just to prove structural completeness.
3. Persist a versioned validation result with completed-cache identity; invalidate
   older cache entries when they cannot meet the stronger acceptance contract.
4. Make init-only/missing-required-media output, truncated final atoms, wrong/missing tracks,
   cancellation, and post-validation mutation fail closed before cache publication.
   Valid short audio/video must remain accepted. Final verification currently
   starts a fresh timeout (`remux.rs:1499`); cap it by the remaining original job
   budget as well as the verification timeout.

**Accept when:** the actual publication path rejects the reproduced malformed file,
accepts valid mixed-copy/encode and HDR fixtures, withholds Complete/reusable stamps
until final checks succeed, and preserves validated growing-fragment delivery.
Validation cost must be measured and bounded. This is a validation
gap, not evidence that normal successful FFmpeg jobs routinely produce bad output.

### R03 — Bound MSE startup, transport, and append waits

**Implemented.** Playlist/init/fragment headers and streamed bodies have progress
and absolute deadlines and byte/work bounds; sourceopen and append/remove waits
are bounded. Source-owned first-frame and playback-progress watchdogs distinguish
preparation, deliberate pause, background suspension and seeking. Failures use
shared recovery and finite retries, preserving the single HEVC/MSE producer
adoption attempt and cancelling abandoned generations. Browser regressions cover
every stalled phase, cleanup, state preservation, terminal retry exhaustion and
successful recovery with actual decoded media. The final complete four-worker
matrix passes with the skips and helper-crash limitations recorded above;
isolated reruns do not replace that gate. See
[browser deadlines and recovery](docs/WEB_PLAYER.md).

**P1 · Reproduced · M.** `crates/server/web/media-source.js:38–87` waits for media
events and SourceBuffer completion without deadlines, and its fetch/body waits
have only source-replacement cancellation (`:146` onward). The player startup
watchdog explicitly excludes MSE (`player.js:1284`). In a routed Chromium harness,
holding the first playlist response while returning producer status `ready` left
the UI on "Preparing media" after 30 seconds: one media request, 39 status polls,
no retry/error. Polling health did not prove playback progress.

1. Give playlist/init/fragment fetches and body reads explicit progress/absolute
   deadlines; bound sourceopen and append/remove completion waits as well.
2. Cover MSE with a source-owned first-frame/no-progress watchdog that recognizes
   buffering, a deliberate pause, and pending seeks correctly.
3. Send timeout/decode failures through the existing single in-flight recovery and
   finite retry budget. Abort replaced client requests and clear source timers/listeners;
   cancel abandoned/terminal generations. Preserve the existing one-attempt HEVC/MSE
   reattachment that adopts the same producer (`preservePreviousTranscode`) instead
   of unconditionally cancelling useful producer work during recovery.
4. Bound streamed playlist bodies even when `Content-Length` is absent; do not
   rely solely on a response header to cap `response.text()`.

**Accept when:** stalled headers/bodies, missing sourceopen/updateend, and bytes that
never decode terminate or recover within an explicit budget; a healthy slow source
gets the intended grace; pause/rate/seek state and stale-session tests stay correct.
Validate Chromium, Firefox, WebKit and mobile coverage where each delivery mode is
available, and state intentional codec/API skips separately.

### R04 — Control active jobs without reopening a vanished source pathname

**Implemented.** Status, timing and cancellation resolve existing generation and
request ownership without reopening the source pathname. Matching DELETE remains
idempotent after removal/rename; fresh admission still requires current catalog
and root-confinement checks. Regressions cover preparation, playback, seek
replacement, disconnect, stale requests and another viewer. An `idle` control
response means no matching generation, independently of pathname availability.
See [the browser API contract](docs/WEB_PLAYER.md).

**P2 · Code-backed · S/M.** `crates/server/src/web_ui.rs:2265–2312` requires the
catalog item and a newly opened confined source file before processing status,
timing events, or DELETE cancellation. Once a title is removed or renamed, the
route returns 404 before `cancel_web_request`, even though an existing generation
owns a validated source descriptor. Every successful poll also repeats rooted I/O.

1. Resolve control requests through the existing generation/session/request owner
   first. Keep new media admission tied to current catalog/confinement checks.
2. Make DELETE idempotent for its matching generation after source removal and
   ensure a stale or different request cannot cancel another viewer's job.
3. Serve active status and record timing without repeated source opens; expose
   source disappearance separately from producer liveness where appropriate.

**Accept when:** remove/rename during preparation, playback, seek, and disconnect
still permits owned cancellation; stale-session and multi-client protections pass;
per-poll source-open work disappears. Existing reconnect grace limits the duration
of abandoned work, so this is not a claim of indefinitely leaked producers.

### R05 — Make release and toolchain pins consistent

**Implemented.** Following the user's later request for latest stable Rust, all
current pins use **1.98.1**, verified against the official stable compiler manifest
and exact digest-pinned Docker image. The trixie compiler image supplies FFmpeg
with seekable descriptor support to isolated Compose tests; the production
FFmpeg 8 runtime is unchanged. One pin inventory drives validation, updates and
workflow staging. Metadata resolution and complete staging precede mutation,
with rollback on publication failure. Ten isolated Python fixture tests cover
successful updates, Docker/Compose/workflow drift, metadata failures, rollback,
concurrent edits and correct compiler-table parsing. Ordinary CI checks the
contract without release tags; `scripts/release-contract.sh v0.1.0` passes without
publishing an image. See [release/toolchain maintenance](docs/DISTRIBUTION.md).

**P1 · Reproduced · S.** `Dockerfile:5` selects Rust 1.98.0, while
`rust-toolchain.toml:2`, `Cargo.toml`, `docker-compose.test.yaml:11`, and the
CI/release workflows select 1.97.1. `scripts/release-contract.sh:25` rejects the
current Dockerfile. The canonical gate passed because it does not run this release
contract; a release tag reaches an avoidable validation failure.

The update path also omits `docker-compose.test.yaml` from
`scripts/set-rust-version.sh:33` and the workflow's `git add` list at
`.github/workflows/rust-toolchain-update.yml:88`. In an isolated copy with the
initial Docker mismatch repaired, the actual updater returned success moving
1.97.1 to 1.98.0 while both the Compose image and exact compiler assertion remained
1.97.1. Its later quality gate would reject that incomplete update.

1. Choose one supported Rust version and update compiler version, image digest,
   exact Compose compiler assertion, workflows, and contributor documentation together.
2. Make the updater cover the complete pin inventory and fail before partially
   mutating files if it cannot resolve all required metadata.
3. Add a version-consistency check to ordinary CI, independent of having a release
   tag/changelog entry. Test the updater against a temporary repository fixture.

**Accept when:** current release contract succeeds; a simulated next-version
update changes every required pin; deliberately drifted Docker/Compose pins fail
ordinary CI; `./scripts/agent-verify.sh` passes. No need to publish an image to
verify the source-side consistency fix.

### P01 — Establish end-to-end preparation and playback benchmarks

**Implemented; measured CPU subset, broader tiers remain unmeasured.** Browser
selection/seek records include capability negotiation and presented-frame
completion; estimated completions remain separate. Server preparation,
source/tool identity, cache/registry wait, admission, helper attempts and the
first complete fragment have bounded records and fixed histograms. Existing
metrics remain; exported records contain no paths, titles or request identities,
and elapsed browser durations never subtract a server clock. The temporary-fixture
harness records actual delivery/recipe, output checks, cache state, environment,
resources, cancellation and variability. Small-sample tail values are explicitly
descriptive. See [measurement usage and limits](docs/WEB_PLAYER.md#playback-measurements)
and the bundle B verification above. No encoder quality or cache identity changed.

**P1 · Code-backed measurement gap · M.** Existing metrics already record initial
bytes, playlist availability, MSE stages, canplay, and playing. However,
`crates/server/src/remux.rs:141–168` stores only count/sum/max, and the job clock
starts after source/tool preparation, cache maintenance, and admission (`:1909`).
`status.rs:313–378` therefore cannot attribute the entire user-visible wait or
derive startup percentiles. HTTP handler duration ends before streaming delivery
(`http_app.rs:678`, `lifecycle.rs:1555–1564`). Initial 16 KiB availability is not
equivalent to a complete fragment or a displayed frame.

1. Start a correlated, bounded timing record at browser selection and server
   preparation entry. Measure source open/sample, tool fingerprint, capability
   negotiation, queue/admission, cache work, each helper attempt, first complete
   fragment, first presented frame, and seek completion separately.
2. Add fixed-cardinality histograms and cache hit/miss/fallback reason counters;
   retain current fields for compatibility. Avoid title/path/request-ID metric labels.
3. Build a reproducible generated-fixture benchmark that records commit/build,
   FFmpeg/libav/tool versions, CPU/cgroup limits, GPU/driver, filesystem/cache state,
   actual output recipe and quality. Keep reports and generated media outside Git.
4. Benchmark cold and warm source/tool/output caches separately from attachment
   to an active producer. Record wall time, CPU time, threads, memory, I/O bytes,
   cache growth, GPU decode/encode/compute/VRAM, and cancellation latency.

**Required matrix:** original; video+audio copy; video copy/audio encode; video
encode/audio copy; both encode; P7/P8 conversion; SDR/HDR10/HLG/Dolby Vision;
24/30/60 fps; representative 720p/1080p/4K and bitrates; zero/near/far seek;
1/2/4 simultaneous viewers; scanner/artwork load; 1x/2x playback and pause.
Use a small CPU fixture subset in CI and larger/dedicated hardware tiers separately.

**Accept when:** reports include p50/p95/p99 selection-to-frame and seek-to-frame,
per-stage costs, sustained speed and resource usage; baseline noise is characterized
and regression thresholds are set per workload. No improvement is accepted solely
because an FFmpeg command finishes faster while output quality or playback worsens.
This work can run alongside R01–R05; it is the prerequisite for changing defaults.

### P02 — Remove repeated cache scans from the global job lock

**Implemented.** One on-demand inventory sweep per 30 seconds replaces repeated
per-producer discovery. Incremental growth/publication/deletion accounting and
fresh free-space checks preserve admission. Exact-path reservations plus a live
registry ownership check protect eviction; slow discovery and unlinking do not
hold the job registry. Images share quota coordination. Raw/HLS/MSE recency
touches only the validated stamp, throttled to once per minute, preserving output
metadata and cache validation. Concurrency regressions cover blocked discovery,
stale snapshots, admission/publication/cancellation, external deletion and recency.
The actual maintenance API scale check covers 100/1k/10k/100k entries and 1/2/8
callers. At 100k entries/eight callers, 72 between-sweep calls had p50/p95
0.023/0.056 ms versus baseline 3326.985/5868.104 ms; standard deviations were
0.018 and 1725.856 ms respectively. One initial inventory discovery took
1191.732 ms; nine baseline single-caller scans had median 670.918 ms, maximum
1148.830 ms and standard deviation 156.769 ms. This does not establish reliable
cold-sweep tail behavior or a cold-start speedup. These are maintenance timings,
not playback speedups. Inventory uses
O(entries) memory and the maintenance caller can still wait on storage. See
[cache operations](docs/OPERATIONS.md).

**P1 · Measured isolated cost · M/L.**
`crates/server/src/remux/cache.rs:110–225` enumerates/stats the cache and sorts
completed candidates even without eviction pressure. `:302–308` holds the global
job registry mutex throughout that maintenance. Each producer invokes it roughly
once per second (`remux.rs:1238–1264`), as do admission, the first transition to
Growing, preprocessing, and finalization. Image misses also take the shared maintenance mutex
(`http_app.rs:2495`, `:2565`). Existing active resource reattachments avoid starting
their own scan, but can still wait on another producer holding the registry lock.

An optimized harness using the extracted production scan/classifier functions,
generated one-byte cache entries, and nine warm runs measured these local medians:

| Entries | Cache scan median |
| ---: | ---: |
| 100 | 0.326 ms |
| 1,000 | 5.277 ms |
| 10,000 | 59.693 ms |

This is a filesystem/function microbenchmark, not a production playback result;
real cache stamps, cold storage and contention differ. The code nevertheless does
approximately O(J × N log N) maintenance work for J producers and N cache entries,
and a blocked scan delays the producer's next cancellation observation.

1. Add timing/operation counts from P01; avoid sorting when reclamation is unnecessary
   and coalesce concurrent sweeps as an initial contained improvement.
2. Introduce incremental accounting for committed and growing artifacts plus one
   bounded sweep cadence. Track active growth without rediscovering every old file.
3. Move slow discovery/eviction outside the job registry lock using reservations
   or generation-checked ownership. A copied protected-path set alone is unsafe
   if a new reader/producer registers after the snapshot.
4. Retain fresh quota/free-space admission before first exposure and publication;
   reconcile external deletion/failure and keep image/video ownership coordinated.
5. Record throttled last-use recency across raw MP4 and HLS/MSE. Currently only
   `serve_finished` touches mtime (`remux.rs:2968`); HLS branches before it
   (`:2473–2482`). Once active protection expires, frequently used HLS cache entries
   can still look old to age/LRU eviction. Protect a validated requested candidate
   during admission and keep content validators independent of recency.

**Accept when:** scan frequency is bounded independently of producer count; warm
attachment/status latency does not grow behind full-directory scans; 100/1k/10k/100k
cache tests cover 1/2/8 jobs, concurrent images, blocked metadata I/O, stale snapshots,
quota pressure, publication races, and HLS/raw recency. No active file is evicted
and accounting converges after failures. Compare tail latency and cancellation to P01.

### P03 — Start linked playback before the full library finishes loading

**Implemented and measured.** Linked item metadata and first-page capabilities
start selection independently of complete list publication. The item API adds a
generation and optional generation precondition; navigation epochs, aborts and
generation checks prevent stale selection. Linked queues remain singleton, card
queues retain their full snapshot, and later-page errors do not block a valid
link. Holding page 2 or 50 is covered in all browser projects. With generated
320×180/24-fps SDR H.264 original media and 600-ms routed page latency, eight
samples per workload measured median link-to-presented-frame 806→171.75 ms for
400 cards and 5253→161.45 ms for 10k cards. Standard deviations were
8.39→12.68 ms and 128.11→15.93 ms respectively. Every after sample presented a
frame before held-page release; no baseline sample did. These controlled browser
results do not establish hardware, storage, CPU/memory or reliable p99 gains.

**P1 · Reproduced dependency · S/M.** `crates/server/web/app.js:355` waits for the
complete library request before selecting a linked item. `api.js:84–130` fetches
every page (50 requests for 10,000 cards), and library capabilities become available
to selection only after that completes. In a 400-entry routed browser reproduction,
the linked item's metadata had arrived but holding page two produced zero media
requests; releasing it started playback.

1. Publish the first page's capabilities independently of complete list publication,
   or add a small additive capabilities response with a consistent generation.
2. Negotiate and start the linked item as soon as its own metadata/capabilities
   are available; finish loading the library in the background. Preserve linked
   playback's singleton queue and ordinary card selection's full snapshot queue;
   expanding a linked queue is a separate product choice.
3. Keep navigation epochs/abort signals and ensure unrelated later-page failures
   can report a library problem without preventing valid linked playback.

**Accept when:** holding page 2 or 50 does not delay the media request; changing
capabilities/navigation invalidates stale selection; queue snapshots, Back/Forward,
history and ordinary card playback remain correct. Compare link-to-frame at
controlled latency for small and large libraries. No full virtualization is needed.

### P04 — Reuse buffered compatible playback for nearby seeks

**Implemented for MSE.** The client retains the source only with matching track,
quality, negotiation/source ownership and an unexpired lease, plus a buffered
target and known decoder prerequisites. Server fragment metadata requires
continuous decode times, known sync flags and bounded reordering. Copied video
retains the preceding random-access point and two seconds of forward decoder
margin; uncertain timing, gaps, eviction or source changes restart. Tests cover
real reordered copied video, nonzero source offsets, paused/rapid seeks,
captions, loop, rate/volume and source replacement, and assert no cancellation,
initialization fetch or new generation on eligible seeks. Native HLS/raw seek
semantics are unchanged. Playback measurements and limitations appear above.
The matched playback run also exposed unnecessary restarts from a status poll
that preceded playlist registration and from transient owned buffer operations.
Pre-registration idle responses no longer establish source expiry; later idle
responses still do. Busy eligible seeks wait at most 100 ms, retain accumulated
relative intent, and recheck ownership and actual decoder data before reuse.

**P2 · Code-backed · M.** `crates/server/web/player.js:368–427` always tears down
compatible playback and waits a 400 ms debounce before reloading, even when the
requested time is buffered. Same ten-second bucket seeks can reuse server work,
but still reset the media element/MSE and fetch initialization again.

1. Add a same-source MSE seek fast path when the target and required decoder margin
   are present and stream/track/quality semantics match. Map global to local time
   explicitly and preserve paused/playing intent, progress, loop and captions.
2. Keep the existing restart path for missing/evicted ranges, gaps, changed tracks,
   and incompatible sources. Evaluate native HLS/raw reuse separately.

**Accept when:** a nearby buffered seek creates no DELETE, new generation, helper,
or init fetch; target accuracy, reordered copied video, paused seeks, rapid direction
changes, captions, end/loop and source expiry tests pass. Measure buffered seeks
separately from cold restart seeks. Depends on R03's reliable progress handling.

### P05 — Reduce cached-stream read scheduling and cursor contention

**Implemented and measured.** Pinned `Arc<File>` positioned delivery removes
shared seek-cursor and index-lock contention. One 256 KiB buffer per socket
starts with a 64 KiB read; EOF alone triggers metadata checks. Cancellation
interrupts slow writes. Same-byte concurrent ranges/segments, publication,
replacement, truncation and producer failures are covered. Matched synthetic
CPU/GiB fell 56.9%; this is not a whole-server throughput claim. See the bundle C
measurements. Original delivery and zero-copy remain unchanged.

**P2 · Code-backed, end-to-end gain unmeasured · M.** `RemuxJob.output` is a shared
`Mutex<File>` (`crates/server/src/remux.rs:287`). `stream_growing` (`:3176–3268`)
schedules a blocking task, locks, stats, seeks, and reads per 64 KiB chunk; HLS
indexing uses the same file mutex. At 100 Mbit/s this implies about 191 chunks/s;
at 1 Gbit/s, about 1,907. These are arithmetic operation counts, not measured limits.
Original delivery also uses a bounded 64 KiB async file loop (`lifecycle.rs:1606`).

1. Use positional reads on the pinned descriptor and separate indexing state from
   delivery cursors. A cloned Unix file handle alone does not create an independent
   seek cursor. Avoid reopening a mutable pathname.
2. Benchmark larger bounded reads/batches and reduced metadata calls; retain
   cancellation, backpressure, socket deadlines and bounded per-client memory.
3. Treat Linux zero-copy serving of immutable completed files as an optional
   experiment after simpler locking/scheduling fixes, with a portable fallback.

**Accept when:** concurrent ranges/segments are byte-identical, readers survive
publication/rename, truncated/failed producers end correctly, slow clients do not
retain unlimited resources, and CPU/GiB/context switches improve on P01 workloads.
P02/P06 should remove unrelated lock contention before attributing gains here.

### P06 — Bound native-HLS history costs and reuse validated indexes

**Implemented for CPU/index costs; native network limitation retained.**
Cumulative timing, immutable 256-entry chunks and detached formatting avoid
whole-history summation/copying under locks. A 16-entry/16 MiB process-local
completed-index LRU binds output identity and parser version after validated
completion; restart reparses. Parser/allocation limits and a 100k-fragment cursor
cover eight-hour shapes. Native targets are stable per session/request; a later
larger copied GOP requires a new generation, preserving old segments. EVENT
network bytes are unchanged; seamless native Safari recovery is unvalidated.

**P1 at long-title scale · Measured metadata cost · M/L.**
`crates/server/src/remux/hls.rs:186–244`, `:275–296` renders the complete native-HLS
history on each refresh. Duration queries also sum vectors. `remux.rs:2595–2621`
holds file/index locks while updating and rendering. A reopened completed output
starts from `Index::default` (`:1822`) and reindexes every fragment. Active indexes
already scan incrementally, and MSE responses already cap pages at 256 fragments.

Synthetic metadata-only index fixtures with 200-character resource URIs yielded:

| Fragments | Warm index median | Full manifest bytes | Render median |
| ---: | ---: | ---: | ---: |
| 600 | 2.991 ms | 151,105 | 0.257 ms |
| 7,200 | 22.095 ms | 1,816,701 | 1.732 ms |
| 28,800 | 87.076 ms | 7,281,501 | 6.947 ms |

These fixtures are intentionally not decodable media; they isolate index/render
scaling. The first MSE page stayed 64,595 bytes. Do not assume these URI lengths,
fragment sizes or refresh intervals apply to every movie.

1. Store cumulative durations/target duration; release file locks after parsing
   new metadata and render from a small immutable snapshot or cached prefix.
2. Reuse/persist compact completed indexes keyed by validated immutable output
   identity, with atomic publication, a parser version, bounds and stale rejection.
3. For native-HLS network reduction, design a compatible window/delta strategy
   and seek contract. Simply removing entries from an EVENT playlist is not safe.
   Keep this protocol decision separate from easy CPU/index improvements.

**Accept when:** 10-minute/two-hour/eight-hour fixtures have measured manifest
bytes/minute and warm-index latency; old/new seeks, native Safari delivery,
dependent copied fragments, truncation, target-duration and stale-index tests pass.
Keep target duration stable within a playlist generation while correctly accounting
for variable copied-GOP segment durations; make this explicit in any window design.
Concurrent segment reads must not wait behind complete-history formatting.
Coordinate parser reuse/versioning with R02 and cache lifecycle with P02.

### P07 — Budget helper resources and producer pacing by workload

**Prototype measured; production defaults unchanged.** The opt-in server
`resource_budget` example compares FIFO/automatic threads, FIFO/bounded threads,
and resource admission/bounded threads under the same four-helper ceiling.
The mixed CPU/NVENC workload found substantial encode and queue regressions,
so it does not justify new defaults. Seven policy tests cover fair turns,
resource ceilings, deadlines and shared-viewer synthetic demand at 2×/pause.
Real FFmpeg pacing remains 30-second burst then 1×; sustained-rate and pause
limitations are not presented as completed production fixes.

**P2 · Code-backed policy; throughput gain requires experiments · L.**
`remux.rs:1862–1888` assigns equal title/helper slots to copy, audio-only, CPU encode
and GPU encode jobs. The helper gate is deliberately fair to queued scanner/image
work (`crates/helper/src/gate.rs:117–127`). This is bounded and prevents starvation,
but does not reserve capacity for interactive starts. FFmpeg builders have no
explicit aggregate decode/encode/filter thread budget. AI upscale already has its
own gate. Encoded indexed producers retain slots while paced at 1x after a
30-second initial burst (`crates/transcode/src/lib.rs:1417–1446`).

1. Use P01 to distinguish CPU, GPU decode/encode/compute, I/O-copy, and short helper
   work. Put media scheduling policy in server/transcode, keeping helper generic.
2. Prototype fair reserved interactive capacity and per-resource concurrency.
   Bound CPU threads against cgroup limits; do not solve contention by simply
   raising `max_jobs` or letting background work starve.
3. Measure rate-aware/buffer-demand pacing: a sustained 2x viewer can exhaust a
   fixed 1x producer's initial lead; a paused viewer can cause unnecessary continued
   production. Preserve download/native-renderer semantics and immutable timestamps.

**Accept when:** concurrent scan/artwork/copy/CPU/GPU workloads meet explicit
tail-latency and throughput budgets without starving any class; >1x playback and
pause remain bounded and smooth for representative long titles; resource ceilings,
shutdown, reconnect grace, deadline and helper-load tests still hold. Choose defaults
only from a dedicated representative workload, not the cache microbenchmark.

### P08 — Benchmark selective GPU paths without sacrificing compatibility

**Measured prototype; no-go for production activation.** The supervised
`gpu_graph` example compares software decode, the established CUDA download
boundary and device-resident NVENC input at the same output profile/preset.
RTX 3050 8 GiB, driver 555.42.06, native FFmpeg 6.1.1/libplacebo 6.338.2:
five rotated trials per graph/preset on generated 8-second 1080p24 HEVC8 SDR/AAC
to 720p H.264/AAC-copy measured download→resident median helper duration
1,029.5→951.9 ms (Balanced), 1,131.8→989.7 ms (Fast start), and
970.3→1,013.4 ms (Maximum speed). Respective sample standard deviations were
31.7→80.2, 114.5→59.4, and 39.4→61.6 ms. All 45 helpers completed successfully;
decoded quality checks captured the first trial of each graph/preset. Those
matched CUDA graph pairs had identical 192 decoded frames. SSIM against a lossless
CUDA-scaled reference was 0.994903/0.994903/0.994806 by preset. Preset quality
differences and the software scaler's different pixels are not graph gains.

The same resident candidate **fails after 48 of 96 frames** when generated
HEVC8 begins signaling BT.709 in a later SPS; the established download graph
completes all 96. `gpu_graph --context-change` reproduces this failure. No
production GPU selection, preset, resource policy or cache identity changes.
Bounded probes distinguish unsupported AV1 encode/High10 hardware decode,
invalid device index, address-space pressure and malformed input. A real
driver reset and physical VRAM exhaustion were not performed.

A separate 40-second workload completed 960 frames per output with matching
CUDA-pair pixels/timing, but has only one sample per graph/preset. Sampled
decode/encode/SM utilization reached 73%/100%/49%; peak whole-device VRAM was
256–272 MiB for download and 254–258 MiB for resident. Exact transfer traffic,
cold storage, feature-length playback and HDR10/HLG/P5/P7/P8 candidate display
quality remain unvalidated. These shared-host observations do not support
changing defaults.

P01 Chromium/MSE measurements use the existing production NVENC path, generated
40-second 720p24 MPEG-2 SDR/AAC→720p H.264/AAC-copy, three independent trials per
preset. Cold / validated warm / live attachment / actual cancellation medians
were 563.5/52.7/160.6/354.85 ms (Balanced), 473.2/47.5/166.8/326.55 ms
(Fast start), and 488.5/52.1/144.9/330.70 ms (Maximum speed). Cold sample standard
deviations were 96.02/25.31/48.95 ms. All 63 records and 18 short sustained
windows passed their checks. These are descriptive preset comparisons, not
resident-graph browser gains or reliable tail estimates. Reports remain outside
Git at `/tmp/rustydlna-p08-*`; the complete environment, recipes, sample records,
limitations and report paths are in `/tmp/rustydlna-p08-handoff.md`.

**P2 · Experiment · L.** Current code deliberately software-decodes some H.264/P7
sources (`crates/transcode/src/lib.rs:549–598`) and downloads scaled CUDA or Vulkan
frames for stable encoder input (`:1159–1180`, `:1242–1274`). CUDA HDR fallback can
download full-size p010 and upload it to Vulkan. The fused Vulkan tone-map/scale
path and multiple quality/preset choices already exist. These are specific tradeoffs,
not evidence that enabling GPU decode everywhere is faster or correct.

1. Measure decoder, filter, transfer and encoder costs by source traits. Prototype
   device-resident decode/filter/encode or interop only for tested combinations.
2. Preserve portable fallback and existing mid-stream frame-context fixes; test
   GPU/device changes, unsupported decoders and resource pressure separately from
   malformed input. Include long playback, not just a one-frame smoke.
3. Compare existing Balanced/Fast start/Maximum speed settings at the same output
   resolution/HDR/audio and agreed quality. Do not make a faster low-quality preset
   universal. Any default/graph change must expose quality effects and revise cache
   identity when generated output semantics change.

**Accept when:** dedicated hardware results show improved startup/throughput/cost
with correct color/tag/bit depth, A/V timing and visual quality across 8/10-bit,
HDR10/HLG/P5/P7/P8, changing SPS/color metadata and GPU failure recovery.
NVIDIA documents GPU-resident scaling to avoid host/device transfers; FFmpeg also
notes that acceleration plus memory transfers can be slower. Both are reasons to
measure the exact graph, not assume a result.
([NVIDIA FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html#n-hwaccel-transcode-with-scaling),
[FFmpeg hardware options](https://ffmpeg.org/ffmpeg.html#Advanced-Video-options))

### P09 — Reduce whole-file Profile-8 preprocessing

**Implemented timing repair, stage diagnostics and guarded final-mux delivery;
larger streaming pipeline remains a no-go.** Experiments found a correctness
defect in the former raw-HEVC wrapping path: genuine MEL retimed to VFR lost
1.043 seconds by its last video timestamp despite identical decoded pixels.
The corrected pipeline wraps the original source with `-copyts`, verifies
converted BL VCL/sample association, replaces RPU and drops EL inside existing
MP4 chunks, preserving timing/edit tables. Final audio and video share the
source clock. The cache revision is `profile8-source-timeline-v3`.
No new dependency or parallel media-helper pipeline was introduced.

Real FFmpeg 6.1.1 and 8.0.1 with dovi_tool 2.3.3 pass genuine MEL CFR/VFR
regressions for per-track PTS/DTS/duration against direct remux, all 259 decoded
BL frames, selected offset audio, three seeks and converted RPU bytes. A repeated
5.4-minute MEL source also matches all 7,770 decoded frames and packet timing.
Profile-8 initialization includes `dvvC`/`hvc1`, PQ/BT.2020 and ten-bit video.
FEL residual image information is explicitly discarded by the existing policy;
genuine FEL video and physical Dolby Vision display validation remain unavailable.

Seven stages report bounded elapsed/file-size/I/O diagnostics. Packet rewrite
and signaling count application I/O; child counters are sampled, explicitly
incomplete lower bounds. On the long fixture, packet rewrite read 5,271,535 bytes
and wrote 2,177,153 bytes; signaling separately read 122,005 and wrote 121,857
bytes against a 3,057,941-byte staging MP4. Signaling is not a whole-movie copy.
Five-sample pipeline medians increased from 708.738 ms (SD 3.378) to 792.478 ms
(SD 2.391), an 83.740 ms/11.8% correctness cost, explained by packet rewrite
83.793 ms (SD 2.243). The short pipeline's apparent 47 ms decrease is polling/
scheduling noise and is not claimed as an optimization.

After prerequisite success, final-mux exposure uses the existing bounded copied
segment index and immediate quota check. Full validation and atomic stamps still
precede Complete; pinned failures cannot replace a reader's generation. Five
fresh-server/cache HTTP trials per arm isolate this change using the **same
corrected conversion recipe**: median first-body 955.915 ms (SD 30.517) when
kept private versus 671.020 ms (SD 24.888) while growing, a 284.895 ms/29.8%
improvement. First complete fragment was 956.116→671.203 ms. All cold/warm
outputs were byte-identical (28,675,419 bytes); warm-cache first-body medians
7.519→7.289 ms are reported separately and launch no new conversion. Separate
active-producer attachment samples (one per arm) were 368.831→49.569 ms and
confirmed one producer. Sampled CPU 1.38→1.43 seconds and peak RSS
108,796→109,016 KiB do not establish a resource-cost improvement. These are
shared-host debug-build delivery timings, not presented Dolby Vision frames.

Installed dovi_tool supports raw stdin and `/proc/self/fd/1` output, but accepts
no container timing. Installed FFmpeg's `dovi_rpu` exposes stripping/compression,
not Profile-7-to-8 conversion. A raw pipe alone would reintroduce timestamp loss;
the larger concurrent pipeline is not enabled. Partial conversion can exit
successfully on truncated raw data, and a short `/dev/full` write can also be
reported successful by dovi_tool, reinforcing the need for sample association,
subsequent parsing and final validation. Tests cover malformed mappings, extra/
missing VCL/RPU, valid EOS/suffix SEI, bounded layouts, quota/ENOSPC, stage failure,
cancellation and post-exposure cleanup. Reports, source recipes and binary hashes
remain under `/tmp/rustydlna-bundle-d-p09-av211tol/`; see `stage-summary.json`,
`http-summary.json`, `http-private-results.json` and `http-growing-results.json`.

**P2 · Code-backed cost; pipeline redesign is experimental · M to XL.**
`crates/transcode/src/lib.rs:3789–4008` serially extracts raw video, converts it,
wraps it into MP4, updates signaling, then muxes audio. This performs at least
four video-sized staging writes plus reads; exact I/O depends on the source.
`remux.rs:995–1027` remains Preprocessing until all stages finish. Intermediate
cleanup is already prompt and signaling uses bounded metadata operations; do not
misattribute whole-movie copying to the metadata-signaling function itself.

1. Record each stage's time/bytes and expose useful bounded progress.
2. Evaluate exposing validated final-mux fragments while that last mux grows,
   after prerequisite conversion/signaling succeeded. Preserve quota checks and
   explicit state transitions; this cannot remove earlier whole-file stages.
3. In a separate experiment, evaluate packet-preserving streaming conversion or
   an appropriate bitstream path that removes stages. Reserve every child/pipe
   resource before parallelizing and supervise the entire pipeline as one owner.

**Accept when:** genuine P7 fixtures and long/VFR sources preserve RPU/dvvC/hvc1,
HDR base layer, tracks, duration and A/V alignment; malformed RPU, disk pressure and
cancellation at every stage cannot publish partial output. Raw extraction loses
container timestamp information, so prove timing before replacing the current path.
HDR10 fallback is a visible quality/feature choice, not an equivalent P8 optimization.
Requires P01, R01/R02 and changed-pipeline cache identity.

### P10 — Reuse successful fallback output under its real recipe

**Implemented.** Successful fallback output is validated and stamped under
its exact effective recipe/source/tool/device identity, in the existing reserved
request lookup slot. Only currently offered, stably unsupported fallbacks may
be preferred for one hour; transient/input/unknown failures retry the primary.
New owners recheck registered completed fallbacks; old owners retain pinned
output. Any descriptor pin closes in-process fallback, including pre-body
exposure. Status and Stream details disclose actual codecs/HDR/quality. There
is no global hardware blacklist or cross-plan fallback deduplication.

**P2 · Code-backed opportunity · M.** `crates/server/src/remux.rs:1073–1120` allows
primary/alternative/portable fallback after an unsuccessful helper exit only when
output is below FIRST_BYTES (16 KiB), regardless of actual client exposure.
`output_fell_back` correctly prevents writing a reusable stamp under the original
recipe. A persistently unusable hardware path can therefore be retried and its
successful fallback regenerated on future sessions.

1. Record effective attempt/recipe and a bounded failure category/duration.
2. Publish fallback output only under its own complete plan/tool/device identity.
   Optionally remember stable unsupported-path observations with a bounded TTL and
   invalidation on executable/device/driver changes.
3. Keep transient busy/failure and title corruption separate; one bad input must
   not globally disable hardware. Preserve the new-generation requirement after
   client-visible bytes; FIRST_BYTES alone is a coarse readiness threshold.

**Accept when:** repeat stable hardware failure reuses correct fallback output,
while transient failures can recover; changed tools invalidate observations; no
SDR/portable artifact is served as requested HDR/hardware output. Validate the
existing early-status/output-pin fallback race regression. Depends on R02/P01.

### P11 — Align MSE byte limits and prioritize media over preview preloading

**Implemented with explicit resource-limit handling.** Server MSE advertisement
and resources share the client's 32-MiB bound; native HLS retains its distinct
64-MiB bound. Oversized copied fragments are rejected as resource limits without
splitting decoder dependencies or silently selecting lossy output. The client
accounts whole retained fragments against a 96-MiB estimate and preserves bundle
A's bounded streaming reads/deadlines, including absent Content-Length. Quota
retries prune only safe ranges and retain paused-seek intent. Preview manifests
and speculative sheets wait for a presented frame plus three seconds buffered;
the active scrub target can proceed. One fetch/decode worker, one latest scrub
target, eight speculative references and two retained images bound work. JPEG
dimensions are checked before decoding; existing images and the next decode
share a 64-MiB RGBA/data-URL estimate, plus one 16-MiB compressed body and a
256-KiB header buffer. Boundary/quota tests and 100
rapid targets cover those limits. This is an application estimate, not a hard
browser-process memory limit; real bursty UHD/device tiers remain unmeasured.

**P2 · Code-backed bounds mismatch/opportunity · M.** The server allows resources
up to 64 MiB (`crates/server/src/web_ui.rs:85`, `:2591`), but the MSE browser rejects
fragments above 32 MiB (`media-source.js:245–248`). A legal server resource can thus
fail in the player. Ten seconds of ahead buffering is a duration bound, not a
byte bound for bursty UHD copies. Absent Content-Length, some allocations happen
before the browser can reject oversized data.

Preview preload starts with item selection/manifest arrival (`player.js:268`,
`:2555–2569`) without waiting for playback readiness. It fetches up to eight sheets
(`api.js:166–193`); each accepted sheet may be up to 16 MiB. That is a 128 MiB
upper bound, not typical measured traffic. The two-reference decoded-image cache
does not cap all outstanding scrub fetch/decode work.

1. Define one explicit MSE resource budget and ensure fragmentation can satisfy it;
   cap body consumption while streaming and classify byte-limit failures separately
   from codec incompatibility. Retain original/HDR where possible.
2. Track buffer byte estimates and duration, adapt eviction under quota pressure,
   and test copied UHD bursts without silently forcing a lossy rendition.
3. Delay speculative previews until first frame and adequate media buffer, except
   the current active scrub target. Bound outstanding fetch/decode count and bytes,
   deduplicate work, and give optional traffic lower priority.

**Accept when:** just-below/at/above-limit and missing-length bodies are bounded;
quota pressure/paused seeks recover correctly; cold playback with slow previews
starts promptly; 100 rapid scrub targets retain only the specified bounded work.
Coordinate R03, P01 and P06; native HLS may need a distinct documented resource budget.

### C01 — Eliminate quadratic scanner allocation and catalog restoration

**P1 · Code-backed with operation-count reproduction · M.** Every new child calls
`next_child_seq` (`crates/scan/src/db.rs:3057–3075`, `lib.rs:4156`), which scans and
parses all existing sibling IDs. Adding N children to an empty folder entails
N(N−1)/2 row visits. A separate path, `db.rs:4252–4264`, linearly checks each existing
child before inserting a unique database object into its parent's vector. The
All Video aggregate makes that quadratic even when physical folders are sharded.

An in-memory reproduction of the allocation query/algorithm visited 499,500 rows
for 1,000 children, 1,999,000 for 2,000, and 7,998,000 for 4,000. These operation
counts are the useful evidence; Python elapsed times are not Rust/server timings.
For 50,000 children, allocation alone implies 1,249,975,000 row visits.

1. Initialize a transaction/session-owned next-suffix allocator once per parent
   from legacy rows, update it with admissions, and preserve existing wire IDs,
   overflow checks and deletion/recreation semantics. A schema change is optional.
2. Build ordered child vectors directly from unique rows where safe; use a temporary
   membership set for seeded/mirrored overlap instead of repeated linear searches.
3. Benchmark cold scan and restart separately, including flat physical directories
   and large aggregate containers. Existing generated libraries shard directories
   and should be supplemented with adversarial shapes, not replaced.

**Accept when:** operation counts grow near-linearly for the changed operations;
1k/4k/50k flat and sharded cases preserve IDs, aliases, rename/rebuild behavior and
deterministic order. Record scan/restart CPU, peak RSS and playback startup under
scan load. This is a better first scalability change than a broad catalog redesign.

**Bundle E implemented.** Writable transactions initialize each parent's suffix
maximum once; TEMP triggers track inserts, moves, deletion/recreation, overflow
and rollback without a persisted schema change. Restoration and bulk patch links
use ordered vectors plus temporary membership sets for seeded/mirrored overlap.
Actual cold scans also exposed parent-first All Video/virtual-folder inode checks;
those now start from existing inode/detail indexes. Allocation VM steps were
164,024→656,024 for 1k→4k admissions; aggregate positive/negative membership
probes were 117,882→471,882. Both scale ~4× for 4× input. Regression coverage
includes aliases, object-only representative changes, rename, cancellation,
transaction/savepoint rollback, external writers, restart, rebuild and overflow.

Final release filesystem scans use 10% hard-link plus 10% symlink aliases:

| Shape / physical files | Cold wall before→after | Process CPU before→after | Peak RSS before→after |
| --- | ---: | ---: | ---: |
| Sharded / 1k | 1.643→1.613 s | 2.07→2.03 s | 102.9→96.9 MiB |
| Sharded / 4k | 7.437→6.453 s | 9.53→8.80 s | 127.9→127.9 MiB |
| Sharded / 50k | 513.148→76.995 s | 542.70→112.50 s | 621.0→547.5 MiB |
| Flat / 50k | 816.215→66.005 s | 845.12→95.10 s | 660.5→589.6 MiB |

Flat 1k/4k final scans took 1.622/5.322 s (CPU 1.83/7.08 s; peak
93.5/130.4 MiB); matching filesystem baselines were not collected at these two
small flat tiers, while matching algorithm workloads cover all six combinations.
Each filesystem scan is one sample on a shared host. The near-linear claim is
for changed operation counts; total scan time also includes probing and I/O.
Ten-process, warm-OS-cache 50k sharded restoration medians were
13.416→6.941 s (sample SD 0.436→0.649 s), CPU medians 13.31→6.885 s,
peak RSS medians 561.1→506.6 MiB. Earlier 1k/4k restoration medians were
183.48→148.54 ms and 590.70→414.30 ms (10 samples each). Flat restoration
is covered behaviorally and by the isolated catalog workload, but no separate
flat process-restart distribution was collected.

Presented-frame startup during a real 4k scan succeeded with verified overlap
in all five final trials per binary: medians 99.2→107.2 ms, means
104.88→109.14 ms, sample SD 17.90→13.93 ms. The modest observed regression
and shared-host variability do not support a startup-speed improvement claim.
Earlier quieter trials were 81.6→81.5 ms. Generated playback bytes match;
quality, delivery and linked-startup semantics remain unchanged.

### C02 — Make database and fallback browser search agree

**P2 · Reproduced expression mismatch · S/M.** `crates/scan/src/db.rs:1691–1706`
Unicode-lowercases the query in Rust while SQL lowercases metadata with SQLite's
built-in ASCII-only LOWER. The memory path Unicode-lowercases metadata
(`web_ui.rs:1242–1253`). Isolated SQLite checks for `ÉTÉ`/`été` and
`ФИЛЬМ`/`фильм` returned no match while the Unicode memory comparison matched.
SQLite documents this ASCII-only behavior.
([SQLite LOWER](https://www.sqlite.org/lang_corefunc.html#lower))

1. Define browser normalization in one shared function and register it for SQL,
   or introduce persisted normalized fields if C04 later justifies indexing.
2. Check search domain parity too: SQL includes full PATH while memory searches
   filename plus metadata. Compare date/episode ordering and tie-breakers as well.
3. Preserve bound/escaped query parameters, aliases and existing SOAP ASCII-fold
   semantics unless that separate contract is deliberately changed.

**Accept when:** actual API and DB/fallback parity tests return identical ordered
IDs/counts across pages for accented/Cyrillic text, combining marks, mixed case,
literal wildcard/backslash inputs and non-UTF-8 paths. Do not silently add linguistic
normalization semantics beyond the agreed contract.

**Bundle E implemented.** SQLite and memory share Unicode lowercase-only
substring normalization, filename/metadata search fields, canonical object title,
physical representative selection, all sort keys, counts and paging. Bound INSTR
parameters preserve literal `%`, `_` and backslash. Actual API parity tests cover
accented/Cyrillic text, combining marks, alias overlays and non-UTF-8 paths;
SOAP matching is unchanged. Exact matching has a measured cold SQL cost (see C04).

### C03 — Bound query wait and execution, including memory fallback

**P2 · Code-backed · M.** Four readers are configured (`http_app.rs:549`), but
`DbPool::read` waits indefinitely on a condition variable (`crates/server/src/lib.rs:559–597`).
Read-only SQLite connections have a five-second lock busy timeout, not a query
execution deadline (`crates/scan/src/db.rs:1507–1518`). HTTP waits for a blocking handler
without query cancellation (`lifecycle.rs:1553–1558`). SQL errors can trigger a full
memory search/sort (`catalog_query.rs:324–333`, `:471–509`). Expensive work can thus
continue after its requester disconnects and saturate readers needed for playback.
The connection semaphore and bounded inputs already limit overall concurrency;
this is not a claim of unlimited request/task creation.

1. Propagate cancellation/deadline through reader admission and SQLite progress
   handling, resetting per-request state on every returned lease.
2. Distinguish unavailable DB from cancelled/over-budget execution. A timeout must
   not trigger another expensive full-memory query; bound fallback work too.
3. Measure reader wait, SQL execution, fallback and timeout separately. Consider
   distinct admission for expensive queries only after mixed-load results.

**Accept when:** cancel waiting and executing queries with all readers occupied;
leases return promptly, a later query is not cancelled by stale handlers, and media
and status remain responsive. Cover generation churn and publication races. Merely
wrapping `spawn_blocking` in an async timeout does not stop its underlying work.

**Bundle E implemented.** One absolute five-second control follows heavy-query
admission (four active queries), reader waits, SQLite progress callbacks,
generation retries and bounded fallback. Lease cleanup removes its callback
before reuse, with no externally retained SQLite interrupt handle. Cancellation,
deadline and work exhaustion cannot trigger fallback or partial success. Memory
queries cap inspected records at 1m and scratch/keys at 64 MiB. Status exposes
fixed-cardinality phase times and stop counts. Regressions exercise occupied
readers, executing SQL, late cancellation, admission, media/status access,
publication, TCP reset and valid FIN half-close. Memory fallback still holds a
read guard while filtering/sorting within its budget; that publication-delay
limit remains. No ownership redesign was introduced.

### C04 — Avoid repeated whole-catalog/folder work for each page

**P2 · Code-backed; improvement size needs profiling · S to L.** SQL web pages
repeat population count, matching count, deduplication/representative CTE and the
page query (`crates/scan/src/db.rs:1697–1746`). LIMIT/OFFSET literals create distinct cached
statements per page. Deep OFFSET still skips earlier sorted rows. Current SQL paging
already avoids cloning the entire catalog in Rust; optimization should build on it.

Physical-folder browser requests follow another path: `web_ui.rs:765–815` and
`:854–900` hold a catalog read guard while collecting/filtering/sorting the entire
folder for each page, then produce only the requested slice. Fifty 200-entry pages
repeat fifty whole-folder sorts for 10,000 children. Conditional generation/ETag
handling occurs after much of the work (`:559`).

1. Profile EXPLAIN QUERY PLAN and VM steps for first/deep pages and distinct queries.
   Parameterize paging values and cache invariant counts by generation/query.
2. Cache or publish immutable ordered folder projections and child counts by
   generation; clone only the requested page and release the catalog lock early.
   Short-circuit unchanged conditional requests where snapshot safety permits.
3. If measured costs remain high, evaluate indexed representative/normalized-key
   projections and additive cursor paging. Preserve offset API compatibility.
   FTS/token search is a separate decision because it changes substring semantics.

**Accept when:** first page, offset 40k, many distinct queries, four clients and
watcher publication preserve representatives, order/counts and generation consistency
on 50k/250k fixtures. Record CPU, allocations, writer wait and page/startup percentiles
for both SQL and physical-folder views. Avoid using warm identical-query cache hits
as evidence that cold/deep paging is cheap.

**Bundle E implemented with measured limits.** Paging parameters are bound;
a generation/query count hint avoids repeated invariant counts, including safe
ui4-wrap rejection. Complete validated pages also reuse IDs/counts in the existing
256-entry cache after the flat 50k warm-page workload still exceeded 250 ms
with count reuse alone. Cold SQL timings below remain separate from page hits.
Physical folders cache ordered IDs (8 entries / 32 MiB) and
child counts (1,024 entries / 512 KiB), copy keys in 256-child lock intervals,
sort outside the catalog lock and clone only the requested page. Evicted IDs
are destroyed after releasing publication locks. Valid unchanged requests
short-circuit before page work. Cache, cancellation and publication races are
covered at the API boundary.

Matched release folder workloads (10 cold and 10 warm samples per case) at
250k entries improved median first/deep/search cold pages from
782.79/849.33/377.47 ms to 160.22/159.30/164.82 ms; warm pages from
772.08/788.10/374.75 ms to 0.867/0.857/0.839 ms. Four clients' 20 distinct queries
fell from 2,096.67 to 1,561.10 ms; maximum observed writer acquisition wait
(20 observations, not a tail percentile or an actual watcher publication) fell
from 836.51 to 101.12 ms. Whole harness CPU fell 54.31→12.85 s; peak RSS rose
1.68% (1,062,012→1,079,864 KiB), including fixture creation and teardown.

SQLite release profiles use five samples per case plus separate EXPLAIN/VM
runs. At 250k, empty first-page medians were 623.17 ms before,
760.31 ms cold after, and 470.35 ms with reused counts; offset 40k was
625.07/778.88/500.01 ms. Reused counts cut ~50.25m to ~30m VM steps.
Cold varied substring queries regressed about 2–2.35× because exact Unicode and
canonical-title matching do more work; `title 04` was 294.43/655.44/326.38 ms,
and `title 2` 342.05/710.47/361.62 ms. Known-zero counts short-circuit misses.
Final 50k server warm-page means after complete-page reuse are
0.538/0.486/0.191 ms (sharded first/deep/search) and 0.496/0.466/0.196 ms
(flat), with 20 timed samples after ten warmups. All six final filesystem tiers
pass the existing 250 ms web threshold. These are repeated page hits, not cold
SQL improvements; the prior flat before/intermediate runs that exceeded the
threshold remain recorded.

An additional after-only handler workload applies ten real four-detail patches
per 50k/250k catalog while four clients page. Publication median including wait
was 17.89/127.00 ms (SD 1.55/8.22 ms); exact IDs, metadata generation, cached-empty
invalidation and all 409 retries passed. Each tier had 40 successful client pages
and 30 retried responses. Health medians were 0.183/0.201 ms, detailed status
12.45/78.64 ms; largest observed successful page was 59.10/363.86 ms.
The harness used 13.58 s CPU and 1,082,580 KiB peak RSS, including fixtures.
It invokes real API/publication handlers without network transport.

These measurements justify bounded reuse, and identify a remaining SQL cost;
they do not validate a particular persisted-index/cursor design. No schema,
FTS, cursor API or whole-catalog ownership redesign was added.

### C05 — Reduce catalog duplication and publication work where measured

**P2 · Code-backed narrow fixes; larger model change conditional · S to L.**
`Catalog.items` stores an owned MediaItem per object (`crates/scan/src/catalog.rs:19–22`),
with path, stream/caption and NFO metadata (`metadata.rs:77–124`). DB restoration
materializes it per virtual alias (`db.rs:4252–4270`). Mirror preparation clones
all browse-folder videos before checking existing mirrors (`catalog.rs:864–871`).
A small patch scans all `by_detail` entries (`:348–349`) under the publication
write lock (`crates/server/src/lifecycle.rs:463–473`).

1. Remove changed detail keys directly and avoid cloning already-existing mirror
   metadata. Measure allocation and write-lock duration before redesigning ownership.
2. If duplication dominates RSS, split immutable shared metadata from object/path
   presentation using typed ownership. Preserve alias-local NFO/probe overlays;
   sharing by physical inode alone can incorrectly merge distinct metadata.
3. Cache generation-level status counts/estimated memory rather than traversing
   and allocating sets for every detailed status call (`crates/server/src/status.rs:50–90`)
   if measurements show status polling matters. Lightweight `/health` already avoids
   the full catalog-count path.

**Accept when:** fixed-size patches do not scan all unrelated detail mappings;
alias-heavy/metadata-heavy memory and lock profiles improve; alias-local sidecars,
deletion of one alias, bookmarks, recent groups and deterministic Browse stay correct.
Retain existing staged disk-backed publication and reusable scan sessions.

**Bundle E implemented narrowly.** Changed detail mappings are removed by key;
existing mirrors avoid metadata/child-vector clones, and physical ancestors are
visited once per distinct parent. Object-only journals retain affected details
for canonical representative reselection. Ten-sample, 1,000-patch isolated
mapping medians at 1k/4k/50k unrelated mappings changed from
187.16/722.43/8,977.77 ms to 2.15/2.32/1.44 ms (final ten-run medians). This is
an operation-specific measurement: full video patch publication still includes Recent maintenance
(~6.2 s for 1,000 patches in the debug workload). Existing mirrors require zero
metadata/container clones; new mirrors retain object/path-local overlays.
Shared metadata ownership and detailed-status count caching were not justified
by isolated attribution of the remaining RSS and publication costs, and remain
conditional. Alias sidecars, surviving aliases, bookmarks, Recent and staged
publication stay covered by scanner/server regressions.

### C06 — Avoid redundant playlist traversal and add cancellation checkpoints

**P2 · Code-backed · S/M.** On full reconciliation or playlist events
(`crates/scan/src/lib.rs:3453–3455`), `desired_playlists` recursively discovers playlists
and unconditionally loads/canonicalizes every detail (`playlist.rs:161–175`), even
when it finds none. Traversal and member loops lack the main scanner's cancellation
checks (`:39–69`, `:161–240`). This wastes I/O on large no-playlist libraries and
delays cooperative shutdown. Ordinary media events do not always take this path.

1. If discovery found no playlists, skip all-detail canonicalization while still
   removing previously published playlists that disappeared.
2. Check cancellation per traversal/detail/member and reuse discovery results or
   targeted member resolution where possible.

**Accept when:** no-playlist reconciliation avoids per-detail canonicalization;
one playlist update does not needlessly resolve every unrelated file; cancellation
rolls back its private stage promptly. Preserve order, duplicate members, encoding,
playlist identity, later media arrivals and alias behavior.

**Bundle E implemented.** Empty playlist discovery skips DETAILS entirely but
still removes disappeared playlists. Targeted updates use known playlist paths,
indexed requested members and relevant inode aliases; ordinary arrivals with no
playlists skip object cleanup too. Member lookup used 22 SQLite VM steps with
both 1k and 4k unrelated details. Cancellation checkpoints cover discovery,
member processing and private-stage mutation. Behavioral tests cover staged
rollback, removal, later arrivals, duplicate order, encodings, aliases and
non-UTF-8 paths. No global playlist/member index or schema change was needed.

### B01 — Fix caption conversion, caption focus, and optional display errors

**P2 · Reproduced · S/M, divisible into small fixes.**

- `crates/server/src/web_caption.rs:241–256` drops empty SAMI sync markers, extending
  preceding text to the next nonempty cue. The actual converter turned text at
  1s / clear at 2s / next text at 5s into a first cue ending at 5s.
- `web_caption.rs:36` accepts `WEBVTTjunk` with a prefix check. The converter accepted
  it but Chromium rejected the resulting track; valid WEBVTT loaded one cue.
- `crates/server/web/captions.js:105–130` rebuilds radio nodes on caption selection. A keyboard
  reproduction selected English with Space and moved focus to BODY while the menu
  remained open. The source-scoped track code also lacks a track-error listener.
- A current-session PiP denial dispatches PLAYBACK_ERROR (`crates/server/web/player.js:2146–2149`)
  even while the source continues playing. The reproduction showed "Playback could
  not continue" with `video.paused === false`.

1. Preserve empty SAMI boundaries, strictly validate WebVTT signatures/timing blocks,
   and check literal angle-bracket/entity text when converting ASS/SAMI to WebVTT.
2. Preserve radio nodes/focus when only selection changes; expose source-scoped
   caption load failure with Off/retry without failing video playback.
3. Handle PiP denial as an optional display failure, preserving the healthy source,
   status, intent and existing late-rejection/session protections.

**Accept when:** real browser cue timing/text matches gap/clear cases; invalid tracks
produce bounded useful messages; Space/Arrow selection retains focus and visible
controls; caption/PiP failure cannot restart or fail healthy media. Keep Axe and
keyboard/mobile/fullscreen tests. These are correctness/accessibility bugs, not a
demonstrated caption script-execution vulnerability.

### B02 — Define recovery after sustained healthy playback

**P2 · Code-backed documentation/behavior mismatch · S/M.**
`docs/WEB_PLAYER.md:444–446` promises decoded playback resets recovery budgets.
The player resets automatic recovery on explicit selection/retry/seek/close or
control changes, but not on sustained playing/time advancement. Separate outages
through a long uninterrupted title can exhaust the same generation-failure budget.
The existing brief-playback test correctly prevents a momentary `playing` event
from restoring unlimited retries.

1. Define a sustained decoded-progress interval that restores the intended recovery
   allowance; evaluate admission and producer-failure budgets separately.
2. Automatically reset only after meaningful healthy progress, not every
   playing/canplay event, observed seek/discontinuity, pause, or single frame.
   Preserve existing intentional user resets on explicit seek/retry. Alternatively
   choose/document a lifetime-per-title policy if that is the intended product behavior.

**Accept when:** repeated brief failures terminate within the existing bound, while
well-separated failures after sustained healthy playback behave according to the
documented policy. Use controlled clocks/events and at least one real decode case;
stale sessions cannot reset another source's budget. Coordinate with R03.

### B03 — Reduce large-list rendering and per-tick UI work

**P2 · Code-backed; budgets/benefit need measurement · M.** The browser retains all
page DTOs, builds all card nodes synchronously (`api.js:84–130`, `library.js:271`),
and temporarily removes containment from every chunk to measure heights on width
changes (`library.js:511–528`). Every playback render makes linear queue searches
(`player.js:890–892`), including time updates. Existing chunk containment and a
four-request viewport artwork queue already reduce paint and network work.

1. Measure heap, DOM nodes, long tasks, input delay and resize/layout cost on
   representative constrained devices. Memoize queue position and update only the
   relevant controls/clock for high-frequency actions.
2. Reduce unnecessary chunk remeasurement, consider lean card DTOs/lazy details,
   and evaluate time-sliced card creation with consistent full-list publication.
3. Treat true virtualization as a separate optional product decision. Preserve
   browser Find, full queue ordering, focus traversal, collection headings,
   screen-reader order and stable scroll geometry promised by the current design.

**Accept when:** large-list interaction/resize meet measured budgets and repeated
navigation/title switches do not grow retained memory. The failed large-list cases
in the full browser run followed by isolated passes justify investigation, but do
not by themselves prove this layout path caused the failures. Instrument it first.

**Bundle E implemented after instrumentation.** Weakly owned queue indexes keep
first-member and immutable snapshot semantics. Clock/preview actions update the
timeline and active chapter without a full control/layout render. Large lists
build privately in batches of at most 128 cards or approximately 8 ms, then
publish coherently; navigation cancels between batches. Matching chunks share
measured geometry with a two-width cache. All cards remain in DOM order for
Find, keyboard traversal, screen readers and collection headings.

Three matched Chromium runs, 10k cards, 4× CPU throttling: median publication
7,201.7→1,793.4 ms, largest long task 5,518→152 ms, initial layout
3,669.7→47.32 ms, first resize 3,115.7→188 ms, return resize
3,161.4→126.9 ms. Two hundred playback ticks fell 358.8→20.1 ms and
8,000→1,400 mutations. Timer scheduling delay fell 6,667.7→174.4 ms;
this is not a trusted-input latency measurement. Five navigation cycles showed
no growing detached card trees or listeners after releasing a benchmark-created
remote DOM handle; heap varied 12.4–13.8 MB before and 13.6–14.5 MB after.
Physical constrained-device testing is unavailable. True virtualization,
lean-DTO/lazy-detail architecture and incremental visible list publication were
not introduced. Focused four-project tests include offscreen focus through resize,
chunk geometry, navigation cancellation and unchanged linked/full-snapshot queues.

### O01 — Supervise and isolate operator conversion jobs

**P1 · Code-backed · M.** The server's helper supervision is substantially stronger
than the separate operator conversion tools. `contrib/library/recode-dv-profile7.py:275`
starts an FFmpeg/dovi_tool pipeline without an overall deadline or process-group
owner; `:533` and `:547` wait on conversions without a timeout;
`:383` probes HDR10+ without a timeout. Several probes use unbounded captured output.
The preview generator already has cancellation, a process group, bounded progress
reads, and an absolute deadline; those protections should guide a small shared
Python supervisor rather than be reinvented differently per command.

`recode_one` also uses `archive_dir/tmp/dest.name` and unlinks any existing file at
that path (`:607–611`); its raw HEVC intermediate is derived from the same stem
(`:528`). There is no per-title/conversion lock. Concurrent invocations or distinct
destinations sharing a basename can interfere with each other's intermediates.
`replace_verified_output` falls back to `sudo mv -f` (`:512–520`), whose cross-device
copy behavior does not preserve the claimed atomic installation guarantee.

1. Introduce a dependency-free Python process owner with null stdin, bounded
   diagnostics, absolute deadline, cancellation, TERM/KILL escalation, pipe draining,
   and complete child/descendant cleanup. Apply first to recoding and probing;
   extend to provider-image decoding and the intake `lsof` call.
2. Use a unique private workspace and per-source/destination lock. Validate
   source identity before final installation and reserve destination names safely.
3. Stage on the destination filesystem, preserve the previous derivative through
   validation, and reject cross-device replacement rather than using a copying
   privileged fallback. Keep original archiving's existing no-replace semantics.
4. Bound diagnostics files as well as in-memory output. Preview diagnostics use
   a temporary file but its growth is not bounded by the retained stderr tail.

**Accept when:** temp-only tests cover hung/stubborn descendant processes,
interruption of either pipeline stage, oversized diagnostics, same-basename
concurrent jobs, full disk, and EXDEV/permission failures. Original and previously
verified derivative remain intact; abandoned intermediates are recoverable.
No test invokes these tools on the user's actual media tree.

### O02 — Preserve operator asset permissions

**P1 · Reproduced · S.** `contrib/library/fetch-dlna-artwork.py:67` describes making
files readable but executes `chmod(0666)`. `place_jpeg` hard-links its source and
then changes the destination mode (`:378–393`), which changes the shared inode's
permissions. A temporary source mode 0640 became 0666 after one call to the actual
function. Preview generation similarly forces 0777 directories and 0666 files
(`contrib/library/generate-dlna-previews.py:818–821`, `:1027`, `:1053`), overriding
restrictive umasks and existing modes.

1. Preserve existing inode modes. Use owner/group write with read access suitable
   for the configured DLNA service; make broader sharing an explicit operator option.
2. If destination permissions must differ, copy to a private temporary inode and
   publish it atomically instead of chmodding a hard link to the source.
3. Review preview lock/workspace ownership together with O01 so unrelated users
   cannot replace writable lock or intermediate entries.

**Accept when:** restrictive umask, pre-existing 0640 artwork, hard-linked sources,
and already-existing preview directories retain the intended write permissions;
the service account can still read published assets. Test on temporary files only.

### O03 — Add durable recovery to multi-file intake

**P2 · Code-backed resilience gap · M.** `contrib/library/lib/intake_media.py:1250`
keeps completed mappings only in memory and rolls back caught exceptions.
`contrib/library/maintain-library.py:172` writes its intake receipt after moving files. Existing
no-replace operations correctly preserve occupied destinations and rollback
collisions, and the guide explicitly says a multi-file plan is not a transaction.
A process crash/power failure between moves therefore still leaves no durable
record of exactly which mappings completed.

1. Write and fsync a bounded intent journal before moving media, recording source
   identity and each mapping. Record completion durably and reconcile it on rerun.
2. Revalidate the planned source inode/size/mtime immediately before moving it;
   planning and provider lookups can leave time for a download or another operator
   to replace a pathname.
3. Keep recovery conservative: never overwrite a concurrent arrival, and expose
   unresolved paths for manual handling. Coordinate locks used by `maintain-library`,
   `update.sh`, and conversion tools without claiming a filesystem transaction.

**Accept when:** kill-after-each-mapping tests recover to a documented state,
source replacements are detected, and all existing rollback/collision tests pass.

### Q01 — Close validation gaps without hiding timing-sensitive failures

**P2 · Reproduced full-suite failures / code-backed coverage gaps · M.** The full
browser run failed in four cases while all four passed with one worker. This
establishes sensitivity to execution conditions, not its cause. Library-scale
readiness waits for rAF sizing after 10,000 nodes attach; whole-list layout is a
candidate to instrument. Retry-budget and WebKit held-frame tests also depend on
timers/media progress. Do not simply increase all timeouts or call the suite green.

1. Capture phase/long-task/resource traces for the four cases under the original
   parallel workload. Separate deterministic state-machine tests from real decoding,
   and establish an explicit concurrency policy for heavyweight scale/media tests.
2. Add the demonstrated regression cases from R01/R02/R03 and B01. Test real
   supported FFmpeg outputs alongside mocked media/status/network events; mocks
   can validate state ownership while missing actual decode/fragment behavior.
3. Extend performance shapes: flat directories, large All Video, many aliases,
   long titles, large cache directories, cold/deep queries, and simultaneous scan,
   image and playback. Keep work-count tests deterministic; use dedicated jobs for
   noisy latency limits and hardware performance.
4. Supplement `scripts/soak.sh` with a persistent-daemon, persistent-cache workload.
   The existing soak repeatedly runs short E2E cycles with restarts; that exercises
   cleanup but cannot alone establish long-lived in-process memory/cache stability.
5. Diagnose the FFmpeg helper SIGSEGV messages logged during the browser fixture run
   on local FFmpeg 6.1.1, and compare the same fixture/profile on production FFmpeg 8.
   Helpers were supervised; this log is not proof of a server crash or the cause
   of the four assertion failures. Keep tool-version-specific results explicit.

**Accept when:** the standard supported browser matrix passes at its stated worker
policy, identified failure causes have regressions, skips remain visible, and a
persistent-daemon run reports memory/threads/FD/cache trends and playback behavior.
Scheduled parser fuzzing and targeted coverage floors already exist; add the changed
parser/cancellation paths rather than substitute a workspace line-coverage percentage.

### Q02 — Keep dependency, build and deployment coverage consistent

**P3 beyond R05 · Code-backed prevention work · S/M.** CI already runs Rust audit,
license/source policy and unused-dependency checks, container scanning, pinned
actions, browser dependencies from a lockfile, architectural smoke, exact-digest
testing, provenance/signing, and promotion. Preserve that substantial pipeline.
The local Rust audit tools were unavailable; npm audit returned no advisories.

1. Add npm to `.github/dependabot.yml`, which currently covers Cargo, Docker and
   GitHub Actions. Include the locked browser-test dependency graph in scheduled
   policy checks; do not conflate test-only dependency risk with server runtime risk.
2. Integrate the complete source-side version consistency test from R05 into normal
   CI and the updater. Record exact FFmpeg/libav/runtime package versions in retained
   media/performance evidence, not just the Rust version.
3. Review clean-builder reproducibility/availability separately from lockfile pinning.
   Docker pins FFmpeg versions but uses live package archives; establish a retention
   or snapshot policy if historical release rebuilds are a required guarantee.
4. For selected gateway/deployment changes, use existing browser-route allowlist,
   container smoke and systemd validation tests. Keep authentication/TLS at the
   documented outer proxy boundary and media mounts read-only.

**Accept when:** scheduled updates cover each actual dependency ecosystem, pin
drift fails before release, evidence identifies tested runtime tools, and clean
build/release checks follow the documented guarantee. No current vulnerability or
licensing violation is inferred solely from a missing local audit tool.

### Q03 — Improve code ownership only where it simplifies selected changes

**P3 · Maintainability opportunity · Scoped alongside implementation.**
`crates/transcode/src/lib.rs` (~7,500 lines), `crates/server/src/remux.rs` (~6,400),
`crates/scan/src/db.rs` (~6,200), `crates/server/src/web_ui.rs` (~4,000), and browser controller/
test files contain multiple responsibilities and extensive tests. Size is not
itself a defect, but it increases the review surface for the planned concurrency,
parser, policy and lifecycle changes.

1. Extract bounded owners as they are touched: effective transcode recipe/tool
   identity/P8 pipeline; server cache/index/job lifecycle; catalog query/projection;
   pure browser recovery/seek decisions. Keep media policy out of `helper`.
2. Move focused tests beside their owner and preserve public re-exports/wire APIs.
   Avoid a single broad reorganization before fixing demonstrated behavior.
3. Evaluate lightweight static JS/Python checks after identifying a concrete class
   of missed errors. They are currently unconfigured; any new tool needs a pinned
   dependency, CI/runtime prerequisite and actionable rules rather than blanket churn.

**Accept when:** each extraction has one clear behavioral owner, preserves public
compatibility and meaningful tests, and makes a selected finding easier to review.
No dependency or API/schema migration should be added just to reorganize files.

## Suggested implementation sequence and dependencies

1. **Correctness foundation:** R01, R02, R03 and R05. R04 is a small related
   cancellation fix. O02 and individual B01 fixes are independent contained changes.
   Run P01 baseline instrumentation in parallel with these fixes; do not postpone
   demonstrated confinement/recovery fixes for an exhaustive benchmark campaign.
2. **High-confidence performance work:** P02, P03 and C01; then P04/P11 after
   source-progress behavior is reliable. Record before/after results using P01.
3. **Scale and delivery:** C02/C03/C06, measured C04/C05/B03, P05/P06. Share one
   representative mixed-load campaign rather than benchmark each in isolation only.
4. **Resource and hardware experiments:** P07/P08/P09/P10 according to the measured
   bottleneck. Keep individually reviewable experiments with an explicit stop/go
   decision; merge only validated combinations and retain portable routes.
5. **Operational resilience:** O01/O03 and B02 can proceed independently with
   temp-only fault injection. Q01 accompanies behavior changes; Q02/Q03 are scoped
   prevention/maintenance work, not a requirement for one giant refactor.

For each selected finding: agree its explicit behavior/performance budget, add a
regression, implement at the owning layer, compare quality and resource results,
and update the authoritative guide. A selection of a performance bundle does not
implicitly authorize dropping HDR/copy, breaking wire IDs, or altering a live library.

## Risks and rollout controls

| Change family | Main risk | Control / rollback criterion |
| --- | --- | --- |
| Demuxer confinement | Rejecting a legitimate admitted input while closing nested reads | Fixture/format matrix on supported libav versions; deny unexpected opens before discovery |
| Cache/index/I/O | Evicting active bytes, accepting stale output, cursor corruption, quota overshoot | Generation reservations, pinned positional descriptors, atomic versioned publication and race/fault tests |
| Fragmentation/seeking/pacing | First-fragment decode, copied-GOP dependencies, drift, 2x starvation | Packet/timestamp + actual browser/renderer tests; preserve existing aligned-seek rules and rollback profile |
| GPU/P8 | Frame-context failure, color/HDR loss, VFR/audio drift, worse throughput | Dedicated long-media matrix, actual recipe disclosure, portable fallback and output-identity revision |
| Catalog/schema | Renumbered objects, broken aliases, mixed generations, migration failures | Stable-ID/parity tests, transactional migration only when needed, representative backup/rollback |
| Browser/UI | Stale state, retry loops, lost focus/intent, broken Find/queue | One source owner, finite budgets, cross-browser behavior/accessibility tests |
| Operator tools | Partial moves, wrong shared permissions, orphaned children or cross-device copies | Private locked workspaces, durable journal, no-replace moves and temp-only fault injection |

Use small independent changes with before/after evidence. For runtime/profile
experiments, retain the existing supported path until the new one meets its
acceptance matrix. Revise cache identities when output or validation compatibility
changes; do not weaken a security fix as a performance rollback.

## Verify

Before implementing a selected item, record its baseline and add a regression that
exercises the failure or its operation-count/performance acceptance criterion.
After implementation, run focused checks and `./scripts/agent-verify.sh`; use the
full browser matrix for playback/UI changes and dedicated media/GPU/soak tests
where the selected behavior requires them. Update the relevant authoritative
guide and bump transcode cache identity whenever old output is no longer reusable.

## Bundle E validation and measurement scope

Bundle E measurements use Rust 1.98.1, SQLite 3.53.2, Node 22.19.0,
host FFmpeg 6.1.1 and Chromium 151.0.7922.34 on the shared Linux Ryzen 9
5950X / 32-CPU / 64-GiB development host. The baseline is the clean
`70df12bd99da247ce7ea3944f18badace1be6b2e` build containing bundle A/B work.
Saved binary hashes, raw samples, standard deviations and workload metadata
are under `/tmp/rustydlna-bundle-e/`; SQL logs are also retained as
`/tmp/bundle-e-sql-profile-counts.log`. Reports and generated media stay outside Git.

Cold scans use freshly generated bounded files with warm OS caches; scans are
single runs per shape/size, not tail-latency samples. Restart uses ten separate
`--database-check` processes against the same retained catalog, including SQLite
quick-check but no listeners or scan. SQL profiles separate VM instruction
counts from five uninstrumented timing samples. Folder profiles use ten samples
per cache condition and record whole-harness CPU/RSS separately. Browser timing
uses three fresh contexts and five navigation cycles per context. Physical
constrained devices, allocator call/byte profiling, privileged CPU perf sampling
(`perf_event_paranoid=4`), and deployment/GPU tiers were unavailable; peak RSS,
explicit capacities, clone counts and operation counts are reported without
claiming an allocation trace. No ownership/index redesign is inferred from them.

The original large-library harness ran standalone reconciliations against the
live daemon database. This advanced its scan epoch and made the next watcher
update trigger stale-stage rejection and full recovery. Those old `update`
values (~18 s at 50k) are recovery measurements, not ordinary targeted-update
latency. Cold scan, restoration and HTTP timings are unaffected by that
interpretation. Corrected runs isolate reconciliation in a SQLite backup and
check daemon logs for genuine targeted publication.

Five corrected alternating 50k sharded watcher trials per binary used the same
frozen database and matching fsynced additions. All ten published directly:
median 429.03→376.82 ms, ranges 418.79–459.85→370.54–450.70 ms, sample SD
15.74→34.30 ms. Preparation medians were 48→22 ms and update CPU medians
0.15→0.13 s. Process peak RSS including startup was 630.6→581.7 MiB
(medians). Fifty-millisecond polling granularity and concurrent validation load
limit the timing claim; startup was excluded. Final unchanged reconciliation
remains a whole-library operation (~15–16 s at 50k).

Final validation: `./scripts/agent-verify.sh` passed formatting, Clippy with
warnings denied, Python/operator checks, all 101 web unit tests, workspace tests,
Rust documentation, administrative smoke checks and all eight explicitly enabled
isolated socket E2E tests. Server tests: 337 passed / 3 ignored; scanner tests:
258 passed / 4 ignored. The applicable full four-project Playwright matrix ran
twice with four workers; each run had 750 passed, 146 intentional browser/media/
fullscreen/platform skips, and zero failures. Relevant ignored allocation,
mapping, SQL, folder and publication workloads were run explicitly. The unrelated
transcode-cache scale test, privileged namespace/Docker/GPU/soak/fuzz and 250k
filesystem-scan tiers were not run. No separate JS/Python lint/typecheck task is
configured; the canonical gate's configured checks all passed.

Initial integration failures were diagnosed and fixed: unused import/test-module
lint, one owned comparison in a new regression, stale representation-revision
ETag expectations, and a missing publication-pool fixture in the opt-in workload.
The first flat baseline threshold failure lost its report in the old fail-fast
harness; a repeat preserved all measurements. The final threshold failures were
resolved with bounded page reuse. An interrupted flat watcher setup produced no
measurement and was replaced by the matching sharded workload. No failed or
skipped run is counted as a pass. Tracked fixtures, toolchain pins, lockfiles,
user media and live configuration are unchanged. No commit, push or deployment.

## Evidence ledger and reproducibility limits

All material conclusions and experiment numbers are recorded above. The following
temporary artifacts retain raw details for this workspace session; they are not
permanent repository documentation and may disappear when `/tmp` is cleaned.

| Artifact | Contents |
| --- | --- |
| `/tmp/rustydlna-bundle-e/measurements.json` | Bundle E final report index, hashes, raw sample references and resource measurements |
| `/tmp/rustydlna-bundle-e/agent-verify.log` | Bundle E final canonical gate, workspace and isolated E2E checks |
| `/tmp/rustydlna-bundle-e/playwright-verified.log` | Bundle E final full four-project matrix (750 passed, 146 skipped) |
| `/tmp/rustydlna-review-agent-verify.log` | Complete canonical gate output and isolated E2E result |
| `/tmp/rustydlna-review-browser.log` | Full 680-case matrix, failures and helper diagnostics |
| `/tmp/rustydlna-review-browser-rerun.log` | Four formerly failed cases passed with one worker |
| `/tmp/rustydlna-review-npm-audit.json` | Zero reported advisories; dependency totals |
| `/tmp/rustydlna-streaming-review.md` | Streaming subreview, exact source references, safeguards and experiments |
| `/tmp/rustydlna-streaming-review-b4i3oei8/` | Generated fMP4 truncation, extracted optimized cache benchmark, actual HLS index harness |
| `/tmp/rustydlna-browser-review.md` | Browser subreview, captions/focus/PiP/deep-link/MSE observations |
| `/tmp/rustydlna-browser-probes.mjs` | Routed Chromium exploratory harness; no live server/library used |
| `/tmp/rustydlna-caption-probe.rs` | Actual converter module harness with protocol-enum shim |
| `/tmp/rustydlna-catalog-protocol-review.md` | Catalog/protocol performance/parity review and coverage |
| `/tmp/rdlna-nested-demux-review-1_054_s6/` | Temporary copied-fixture scanner evidence and rescan logs for R01 |
| `/tmp/rustydlna-review-operator-ksycdsbr/result.json` | Actual artwork hardlink mode changed from 0640 to 0666 |
| `/tmp/rustydlna-review-toolchain-g0dxdu3s/result.json` | Real updater's successful temp-copy update with stale Compose pins |

The original review evidence used Rust 1.97.1, Node 22.19.0, npm 11.19.0 and host FFmpeg
6.1.1. It is not a performance measurement of the production FFmpeg 8 container,
a dedicated GPU run or a user's full library. Microbenchmarks used warm local
filesystem caches and generated inputs; repeat with cold/large/realistic inputs
before claiming deployment gains. No measured GPU speedup is asserted.

Rejected hypotheses worth retaining: completed-cache hits do not re-FFprobe movies;
source hashing is bounded rather than whole-file; active segment requests reuse
prepared identity; MSE playlists already paginate; probing happens in private scan
staging rather than under the live SQLite writer; runtime reuses scan-stage backups;
ordinary media events do not always rescan playlists; SSDP/GENA/helper counts and
queues are bounded. Media GETs intentionally close connections, so an early EOF
in original delivery was not established as a subsequent-response framing defect.
