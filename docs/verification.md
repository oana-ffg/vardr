# First-draft verification

Local verification on Apple Silicon macOS:

- `cargo fmt --all --check`, strict Clippy on all targets, and the release build passed.
- 19 integration tests passed: real CLI import/edit/run, optimistic revisions, schedules and daylight saving, overlap/pause races, cooldowns, timeouts, escaped descendants with inherited pipes and large stdin, bounded logs/history, malformed-job isolation, credential resolution through a fake secret-manager executable, T3 JSON/SSE session negotiation, uncertain dispatch without retries, dashboard entry/API authentication, Host rejection, duplicate-run refusal, graceful SIGHUP shutdown, and signature tamper/wrong-key rejection.
- The actual authenticated browser UI saved a changed daily time while preserving all other job fields. Only schedule, revision, and update timestamp changed. Run now produced a durable success record and the expected output through the real Rust runner.
- The final optimized idle scheduler measured 7872 KiB RSS (7.69 MiB), with no jobs, agent process, UI, or automatic network update check running. This is a single local sample, not a guarantee for every machine or job load.

## Acceptance boundaries

The local T3 adapter was tested against a protocol fixture, not a live billable agent run. It needs a reachable authorized T3 MCP endpoint, actual project/provider identifiers, and durable credentials. It records dispatch receipts; T3 owns later execution results.

The macOS installer generates/installs a user LaunchAgent, but this development run did not install it or migrate existing automations. Linux runs under an existing supervisor. Windows is not supported in this draft. Published binaries have update signatures; Apple publisher signing/notarization is not configured.

The public CI and signed release workflow supply cross-platform verification. Consult their actual run status for a particular commit. Live signed-update acceptance is recorded after the first release is available.
