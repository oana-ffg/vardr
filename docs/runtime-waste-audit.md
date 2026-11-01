# Runtime waste audit

Scope: scheduler idle work, dashboard requests, history/log retention, and the
optional desktop Rust/web paths. Base: `e0716e9cccb711250249fb5e1d90f554d1fc8681`.
No installed scheduler, app, user job, or user state was used.

## Scheduler: unchanged files and unnecessary locks

Every two-second scheduler tick read and validated every job, read every enabled
job's runtime state under a transaction, and read configuration twice while idle.
The automatic updater also read its attempt record on each idle tick when enabled.
Even a manual job took a state transaction although it could never become due.

The scheduler now caches validated definitions, settings, runtime state, and update
state using file identity, size, mode, mtime and ctime (including nanoseconds).
It still inspects metadata on each tick, so CLI edits, in-place edits, atomic
replacements, deletions, and state changes remain visible. Failed reads/validation
are returned as errors rather than falling back to old values. Symlink checks remain
in force. Removed jobs are evicted; the cache holds at most the current inventory.
Non-due and manual jobs take no state transaction. Due jobs still re-read state
under the existing lock and persist their reservation before dispatch. Runner
revision, pause, removal and overlap checks remain in place.

### Measurement

Measured on macOS/ARM64 using Rust/Cargo 1.99.1, debug test builds. Other work
shared the machine, so elapsed times are context, not a speedup claim.

```sh
cargo test --locked --lib idle_scheduler_io_budget -- --nocapture
```

The instrumentation-only commit `94cd32ade7dcca3ac7853ad6c7f8a0008384ceb6`
contains the same test against the original scheduler. The following fix commit
adds zero-read/lock assertions. The test runs the actual continuous scheduler loop
against a temporary Store with 100 hourly jobs and existing state files. Automatic
updates are enabled, with a current-day attempt receipt preventing any network
request or installation. Startup/recovery and the first warm-up tick are excluded;
three real two-second timer waits and three subsequent ticks are counted. Startup,
shutdown, and the unchanged 30-second heartbeat's intentional writes are outside
this window.

| Operation in the measured window | Before (6.050 s) | After (6.033 s) |
| --- | ---: | ---: |
| Job JSON reads/validation | 300 | 0 |
| Runtime-state JSON reads | 300 | 0 |
| Config JSON reads | 6 | 0 |
| Update-state JSON reads | 3 | 0 |
| Per-job lock-file opens/lock attempts | 300 | 0 |
| JSON file replacements | 0 | 0 |
| File/directory `sync_all` calls | 0 | 0 |
| Scheduler timer waits | 3 | 3 |

These are test-instrumented logical operations, not physical disk I/O or kernel
wake-up totals. `fs_usage -w -f filesys -t 1 999999` reported that root was required,
so filesystem tracing was not used. Counters are thread-local and compiled only
under `cfg(test)`; the idle scheduler performs no worker dispatch in this fixture.
There are no performance thresholds based on elapsed time.

Regression coverage also checks same-inode/same-length edits with restored mtime,
atomic replacement, missing defaults, malformed JSON and repair, symlinks, eviction,
external watermark changes, reservation invalidation, and updater configuration
changes. A real CLI test starts a temporary scheduler, edits an already-cached job
with `vardr schedule --every 10`, and checks the actual run and output.

## History: skipped dispatches bypassed retention

`run_scheduled` recorded a skipped occurrence on overlap, pause or revision change
and returned before pruning. A long overlapping manual run, or repeated stale
scheduled dispatches, could accumulate finished records without the configured
limit. Those records also inflated startup recovery and dashboard history scans.

```sh
cargo test --locked --test core skipped_dispatches_obey_retention_without_pruning_active_runs -- --nocapture
```

Applied before the fix, the real runner test recorded 40 finished skipped runs with
`retained_runs=3`; afterward it retained 3. The same active run remained present in
both cases. The test alternates overlap and stale-revision skips. Skipped dispatches
now use the existing pruning path. A second regression ensures an active failure
investigation survives pruning even though its original outcome has a finish time.

## Checked, rejected, and residual candidates

- **Heartbeat fsync every two seconds:** stale report. Current code already writes
  `service.json` on status/error changes and on a 30-second heartbeat. The measured
  steady idle window had no JSON replacements or sync calls before the fix. Those
  intentional durable heartbeat writes were retained.
- **Dashboard polling:** the web page loads once and refreshes on user actions;
  there is no periodic fetch or per-keystroke network request. Search/filter render
  in memory. Request generations discard stale responses; actual outstanding remote
  requests are not cancelled. Repeated explicit refresh clicks can still queue work;
  no request-count measurement established a new fix here.
- **Latest-run lookup:** `snapshot` reads/sorts retained history to select a latest
  run. This is whole-history work on an explicit request, bounded by retention
  (default 30, configurable through 1000 finished runs per job), plus active records.
  No large-history latency measurement was taken. No persistent latest-run index or
  extra per-run durable writes were introduced for this on-demand path.
- **Output and ordinary completion retention:** existing runner tests confirm output
  is capped and old finished runs are pruned. Output is drained in bounded chunks;
  process timeouts and cleanup bound supervision. The skipped-dispatch exception
  was fixed above. Logs can still be large within their explicit configured cap.
- **Launchd/remote discovery:** user-triggered, with one runtime subprocess per
  discovered non-Apple service while the existing discovery time budget permits.
  Capture sizes, subprocess timeouts and directory entry limits exist. Not an idle
  subprocess loop; no discovery against user services was run for this audit.
- **Desktop Rust:** update checks use an atomic single-flight flag and a current-day
  receipt. The update timer checks STOP once per second between minute-scale checks.
  This is a residual wake-up candidate: estimated 60 timer expirations per minute
  from source, not measured in a native app. Native app wake-ups, UI responsiveness,
  and energy use were not profiled; no installed app was launched. The dashboard
  server also retains its existing 250 ms shutdown/worker-reaping wait. These timer
  policies were not changed without native profiling.
- **Cache limits:** memory scales with the current job definitions and runtime-state
  inventory; metadata scans and the two-second scheduler tick remain. No filesystem
  watcher, new service, retained deleted-job cache, or unbounded queue was added.
- No unrelated dead exports/helpers were found. The old one-shot automatic-update
  function was replaced by its used scheduler-owned cache, without an orphan wrapper.

## Validation

Passed:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python3 -B -m unittest discover -s scripts/tests -v
cargo build --release --locked
```

The full test suite uses temporary homes and mock subprocesses/servers. Localhost
socket access was allowed for its mock MCP/dashboard tests. Native desktop packaging,
native UI interaction, and energy profiling were not run. Release/signing workflows
were not dispatched, and no tags, release version changes, deployment or installation
were performed.
