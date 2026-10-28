# Vardr

Know what your automations do, why they exist, when they run, and what happened last.

Vardr is a lightweight Rust CLI and background runner with an optional native macOS app. Agents use the CLI; humans can use the app or local browser dashboard. Closing the app does not stop scheduled automations. MIT licensed.

**First draft, v0.5.2.** macOS app; macOS and Linux CLI. No Electron runtime, Node runtime, database server, or always-open browser is needed. Agent processes run only when a task or failure investigation calls for them. Actual agent memory use depends on the chosen agent.

**Screenshots use synthetic demo data:** names, machines, schedules, run results, and logs are fictional. The macOS app and browser dashboard share this interface.

![Vardr overview showing job purposes, editable schedules, run status, and an enabled failure investigator](docs/dashboard.png)

<details>
<summary>More screenshots: run output, launchd inspection, and mobile dashboard</summary>

**Run output.** Filter jobs needing attention and inspect the latest output. A completed investigation does not change the original failed result.

![Latest run output for a fictional website health check, with its failed status and opt-in investigator visible behind the dialog](docs/run-output.png)

**Existing launchd jobs.** Inspect service status, executable paths, and triggers without changing their owner or schedule.

![Read-only launchd inventory with an expanded definition showing KeepAlive and RunAtLoad triggers](docs/launchd.png)

**Mobile browser layout.** The optional browser dashboard adapts to narrow screens.

<img src="docs/mobile-dashboard.png" alt="Vardr browser dashboard on a narrow screen, with daily schedule controls and run actions" width="360">

</details>

## What works

- Required names and purpose descriptions, project/machine labels, and a working directory for each automation.
- Manual, interval, and daily schedules with IANA timezones and optional weekdays. Pause, resume, reschedule, delete, and run now through the CLI or dashboard.
- Ordinary commands, Codex CLI, Claude Code, and T3 Code launches. Existing Codex/T3 schedules remain independent; Vardr does not modify their scheduler files.
- Durable run records, bounded streaming logs, timeouts that terminate process groups, per-job overlap prevention, and configurable scheduler concurrency (one by default).
- Optional **spawn an agent to investigate failures**, with a separate result/log and a per-job cooldown. Investigations diagnose; they do not automatically repair or retry jobs. The original failure stays a failure.
- Read-only launchd views for this Mac and connected Macs over SSH. See calendar/interval/event triggers, disabled overrides, and runtime snapshots without adopting or changing existing services.
- Signed GitHub Release updates using the same standalone-helper pattern as [Ratatoskr Skill Sync](https://github.com/oana-ffg/ratatoskr-skill-sync). Automatic checks run once per local day, from midnight by default, when the scheduler has no workers active.

## macOS app

Download the Apple Silicon or Intel DMG from [GitHub Releases](https://github.com/oana-ffg/vardr/releases/latest), drag **Vardr.app** to Applications, and open it normally. The window uses the macOS system web view; it does not start a browser, Electron, or Node.

- **Scheduler → Start Scheduler at Login** installs and starts the independent background runner. Opening the app alone does not enable scheduled jobs.
- **Scheduler → Open Data Folder** opens your private definitions, logs, and settings.
- **Scheduler → Copy CLI Command** copies the exact bundled CLI command for this installation. Agents can also call `/Applications/Vardr.app/Contents/MacOS/vardr` directly.
- **Vardr → Check for Updates** installs signed app updates for the next launch. Automatic checks follow the same daily update setting while the app is open. The headless scheduler updates its own standalone helper independently.

Closing the app releases its window and UI process. If a manually started UI job is still running, Vardr asks before interrupting it. Jobs owned by the independent scheduler continue. App and CLI use the same data directory; `VARDR_HOME` also works for an isolated app trial.

The scheduler runs a stable copy of the CLI in the private data directory, so replacing or updating the app cannot remove the service executable. App updates leave existing scheduler jobs and run records alone. Releases are Developer ID signed and notarized. Upgrading from 0.4.x to 0.5.0 is a one-time manual install that moves Vardr to the `com.firefungames.vardr` identity; see the [migration and permission steps](docs/releases.md#one-time-050-identity-migration) first.

To build the macOS app from source (Rust and Node are build tools only):

```sh
cd desktop
npx --yes @tauri-apps/cli@2.12.3 build --bundles app
```

The app is produced in `target/release/bundle/macos/Vardr.app`. Release builds create a drag-to-Applications DMG with `scripts/create-desktop-dmg.py`.

## Try the CLI

Build with Rust 1.90 or newer:

```sh
cargo build --release --locked
./target/release/vardr init
./target/release/vardr put examples/hello.json
./target/release/vardr run hello
./target/release/vardr ui
```

The example is paused and manual. It only prints a greeting. `run` explicitly runs even a paused job. Open the private loopback URL printed by `ui`; it grants access to that dashboard session. Start the scheduler independently with `vardr serve`; the UI itself does not schedule work.

For an isolated trial, pass an absolute `--home` directory on every command or set `VARDR_HOME`. The default is the platform's local application-data directory, printed by `init` and `status`. Job definitions and logs stay in that private directory, outside this repository.

To install the binary from source:

```sh
cargo install --path . --locked
vardr service plan
vardr service install
```

`service install` is macOS-only and installs one per-user LaunchAgent. It runs after login; it does not wake a sleeping Mac or run before the user logs in. On Linux, run `vardr serve` under your existing supervisor. `service remove` removes only the recorded, unchanged Vardr service. No service is installed by building, `init`, or opening the dashboard.

## Existing launchd jobs and connected Macs

Select a Mac at the top of the app to view and edit its Vardr jobs, or inspect its existing launchd jobs. Remote schedule edits, pause/resume, deletion, investigator settings, and output use bounded SSH requests to that Mac’s Vardr CLI. Run now is available through the app or CLI on the target Mac. The native Scheduler menu controls the Mac running the app.

The app’s **Existing launchd jobs** section reads user LaunchAgents, global LaunchAgents, and LaunchDaemons. Apple OS services are excluded. Select a Mac and press **Refresh launchd** to take a new snapshot. An unreadable definition or unavailable host is shown as an error, not an empty healthy inventory.

For another Mac, install Vardr there and configure a trusted SSH alias with key-based access, then run `vardr hosts add studio --ssh studio`. Use `--remote-home /absolute/path` to select an isolated data directory on the other Mac. The default remote CLI path is `/Applications/Vardr.app/Contents/MacOS/vardr`; override it with `--cli` for another installation. Connections are saved privately in `hosts.json`. Existing SSH host keys must already be trusted. Discovery runs on demand over SSH with bounded output and a timeout; it requires no persistent process on the other Mac.

Discovery displays the executable and selected trigger fields, and omits command arguments and environment variables. Trigger paths and executable paths may still contain private information. Calendar times use the target Mac’s local timezone. Disabled overrides take precedence over the plist default. Runtime status uses diagnostic `launchctl print` output, whose format can change; unsupported fields remain unknown. A last exit code is a reported value, not proof that the job ran recently or fulfilled its purpose.

The launchd view is read-only. Vardr-managed jobs on the selected Mac support editing through the dashboard with revision checks. A lost remote edit response is reported as an unknown outcome: refresh before retrying; the request is never automatically retried. Existing launchd jobs stay owned by launchd until an explicit migration. Migrate only triggers and execution settings that Vardr can faithfully represent, preserve a rollback copy, and disable the original schedule before enabling its replacement.

## CLI for agents

```sh
vardr list --json
vardr show hello
vardr schedule hello --daily 09:00 --timezone Europe/Copenhagen
vardr schedule hello --daily 09:00 --timezone Europe/Copenhagen --weekdays 1,2,3,4,5
vardr schedule hello --every 3600
vardr pause hello
vardr resume hello
vardr remove hello
vardr history hello
vardr logs hello
vardr logs hello --investigation
vardr status --json
vardr discover --json
vardr hosts add studio --ssh studio
vardr discover --host studio --json
vardr hosts list
vardr hosts remove studio
vardr update
```

Create jobs with `put FILE.json` or pipe bounded JSON to `put -`. Command targets accept an optional `env` object, applied only to their child process; an omitted/empty object preserves inherited environment behavior. Keep credentials in a secret manager rather than inline job fields. The `bridge` command handles one bounded JSON request on stdin for the SSH-connected dashboard, without starting a server.

Create jobs with `put FILE.json`. To edit a job, fetch it with `show`, edit the returned JSON, and submit it with `put`. The saved `revision` prevents silently overwriting another CLI/UI edit. The dashboard uses the same Rust storage and runner code. Schedule changes retain your other fields. An edit establishes a new scheduling baseline; it does not replay earlier occurrences.

Configure an investigator in the job definition, then toggle it with:

```sh
vardr investigate hello --enabled true
vardr investigate hello --enabled false
```

See [verification and current limits](docs/verification.md), [job definitions and agent targets](docs/jobs.md), [execution semantics](docs/execution.md), and [signed release operations](docs/releases.md).

## Updates

The official verification key and release endpoint are compiled into the binary. No signing-key setup is needed on users' machines. Automatic helper updates are enabled by default when `serve` runs. The app checks separately while open and records its results in `desktop-updates.json`. Edit the local `config.json` to change the time or disable them:

```json
{
  "max_concurrency": 1,
  "retained_runs": 30,
  "log_limit_bytes": 1048576,
  "updates": { "automatic": true, "time": "00:00" }
}
```

The scheduler checks at most once per day at or after that local time, verifies the update's signature and executable version, atomically replaces the binary, and restarts when its own workers are idle. Update failures are saved in `updates.json` and exposed by `status`; they do not stop task dispatch. `update --install` is available with the scheduler stopped. No release published yet is treated as no available update.

## Scope

Each Mac runs its own Vardr scheduler; the app can edit jobs and inspect launchd across connected Macs. Machine labels on managed jobs describe local jobs; they are not remote-routing instructions. Importing/adopting existing launchd definitions, external notifications, event/webhook triggers, dependency graphs, and monitoring a dispatched T3 run to completion are future work. There is no speculative trigger engine running in the background.

T3 launches are recorded as **dispatched**, not succeeded. The receipt links to the actual T3 thread; its execution and approvals remain owned by T3. A failed dispatch can trigger investigation, but a later failure inside T3 currently cannot. T3 integrations support environment or gopass credential references and need an authorized reachable MCP endpoint; a temporary per-turn token is not a permanent scheduler credential.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CLI CI runs on Apple Silicon macOS, Intel macOS, and Linux x64. Desktop CI packages and verifies the app on both Mac architectures. Release signing is build-time tooling only; production requires just the native binary. The optional UI binds only to IPv4 loopback and authenticates its page with a private entry URL and protects its API with a fresh per-process token. No telemetry or hosted service.
