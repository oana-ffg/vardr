# Job definitions

`vardr put FILE.json` validates the whole definition before saving. `vardr show ID` returns the exact saved definition, including the revision required to update it. Unknown fields are rejected.

`vardr remove ID` deletes the definition with its schedule state and run history. It is refused while the job is running, and cannot be undone; use `pause` to stop a job while keeping its history. A job later created with the same ID continues the removed job's revision numbers, so an edit or scheduled run prepared for the removed job is rejected instead of applying to the new one.

Required fields:

- `id`: 1–64 lowercase ASCII letters, numbers, and hyphens. Stable file/CLI identity.
- `name`, `purpose`: nonempty human descriptions.
- `cwd`: an absolute working directory on this installation's machine. A temporarily missing directory produces a per-job run failure; it does not prevent other jobs from running.
- `target`: one of the targets below.
- `schedule`: `{"type":"manual"}`, `{"type":"interval","seconds":3600}`, or `{"type":"daily","time":"03:10","timezone":"Europe/Copenhagen","weekdays":[1,2,3,4,5]}`. Weekdays use Monday=1 to Sunday=7; omitted/empty means every day.

Optional fields: `project`, `machine`, `enabled` (default true), `catch_up` (default false), `timeout_seconds` (default 3600), `prompt` (required for agent targets), `investigation`. The tool maintains `revision` and `updated_at`.

`machine` is a display label. The local data directory determines execution ownership. Do not copy the same enabled schedules to two hosts unless you want two independent executions.

## Ordinary commands

`{"type":"command","argv":["/bin/echo","hello"]}`

Arguments are passed directly, without shell interpolation. An explicit `/bin/sh -c` command is supported if a shell is part of the intended task. The environment is inherited from Vardr. Do not put credentials in argv or public job examples.

## Codex and Claude Code

`{"type":"codex"}` or `{"type":"claude"}`. Both accept optional `model`. Without it, the installed CLI's configured default applies. Install and authenticate the CLI separately.

Prompts go through stdin, not process arguments. Codex uses `exec --sandbox read-only`; Claude uses print mode with plan permissions. These built-in targets are for inspection/research and diagnosis. To run a different explicitly chosen CLI permission/profile configuration, use an ordinary command target. Vardr does not add permission-bypass flags.

Codex working directories normally need to be trusted Git repositories. A supervisor's environment may differ from an interactive terminal. The macOS installer records the installing terminal's PATH, but never copies credentials into its plist. Reinstall the service if executable locations change.

## T3 Code

The target fields are:

- `type`: `t3`.
- `endpoint`: the actual Streamable HTTP MCP URL supplied by your T3 environment. HTTPS, or loopback HTTP only.
- Exactly one credential reference: `authorization_env` names an environment variable containing the full Authorization header, including its scheme; `authorization_gopass` names a one-line gopass entry containing that header. No token value is stored in the job definition.
- `project_id`: the actual T3 project ID.
- `model_selection`: optional selection object using the live T3 provider instance/model catalog. Omit to use the project's default.

Vardr initializes an MCP session, calls `t3_thread_launch` with the project root, prompt, and approval-required permissions, and saves the returned receipt. No worktree is created. It does not borrow a provider's CLI as a substitute for T3 orchestration. MCP JSON and SSE responses are supported, including negotiated session/protocol headers.

The T3 project determines the actual checkout. For cross-machine investigations, the prompt includes the last 16 KiB of the failed run's output, plus its result and purpose; paths refer to the execution host. The investigator may need additional authorized access to that host for deeper diagnosis.

**Credentials must remain valid outside the creating agent turn.** Session-scoped `T3_ACP_MCP_*` values are useful for immediate integration but are not a durable unattended setup. Use an endpoint and authorization provisioned by your T3 deployment. Vardr does not scrape T3's databases or token files. If your T3 environment cannot provide durable external access, keep T3 recurring work in T3's scheduler and use its native tools to call Vardr's CLI for local jobs.

**Installed macOS service:** use `authorization_gopass` for a secret-store entry that the logged-in user can read unattended. The installer preserves PATH, so gopass must be present there, and the normal GPG agent must be able to unlock the entry without a prompt. Resolution uses `gopass show -o`, captures at most 8 KiB in memory, discards stderr, and stops after ten seconds. No resolved secret is written to a log or plist. An unavailable/locked secret store produces a visible failed run. `authorization_env` is suitable when the supervisor already provisions that variable; an installed LaunchAgent does not inherit arbitrary variables from the installing shell. Vardr does not create credentials or change the secret store.

Dispatch is intentionally not retried: T3's thread-launch operation has no idempotency key. After an ambiguous launch response, the run is `dispatch_unknown`, no automatic investigator is launched, and you must inspect T3 before retrying. Vardr's run record does not claim remote completion.

## Failure investigation

`investigation` contains `enabled`, `target`, `cooldown_seconds` (default 3600), and `timeout_seconds` (default 600). Any target above can be used. Only explicitly enabled investigations start an agent. The prompt requests a diagnosis and proposed fix, with no retries, edits, schedule changes, or messages.

One investigation is attempted per failed local run, subject to the per-job cooldown. Attempts are recorded before launch. The investigator has its own bounded output and result. Investigation failure never recursively launches another investigator, and investigation success never clears the original job's failure.

Investigations can transmit the included failure evidence to the configured provider or T3 environment. Enable them only for jobs whose output you want that target to inspect. The log is marked as untrusted evidence in the prompt. Built-in Codex/Claude modes constrain editing; a custom command or T3 tool access still operates within its own authorization system.
