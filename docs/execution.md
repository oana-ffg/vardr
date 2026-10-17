# Execution and resource use

One native process schedules jobs. The UI is a separate, optional native HTTP server that serves a small embedded HTML/JavaScript page. No JavaScript runtime is installed with Vardr. Closing the browser or UI server does not affect `serve`.

The scheduler sleeps between two-second checks. Default scheduled concurrency is one, including failure investigations; increase `max_concurrency` deliberately. Manual CLI/UI runs are explicit and can run other jobs concurrently, but every job has an OS file lock that prevents overlap across all entry points.

Output streams into a per-run file with a default 1 MiB cap. Remaining output is drained and discarded so a verbose child cannot deadlock on a full pipe. Vardr retains 30 completed runs per job by default. Records and investigation output are separate. Logs can contain sensitive command output; the data root is private to the current user on Unix.

## Schedules

- Interval minimum is 10 seconds. Intervals are anchored at registration/edit time and advance by whole intervals.
- Daily schedules use the specified timezone. During a fall-back repeated hour, run once at the earlier occurrence. A nonexistent spring-forward time is skipped.
- `catch_up: false`: occurrences more than 60 seconds late are skipped. A busy machine or concurrency limit can make a job late.
- `catch_up: true`: after an interruption, coalesce missed occurrences into the latest one. Never replay a whole backlog.
- New and edited definitions establish a fresh scheduling baseline. Pausing/resuming does not replay the time spent paused.
- Jobs compete by due time, oldest first. A manual run does not move the scheduled watermark.

This is **at-most-once dispatch**, not exactly-once external effects. Dispatch intent is persisted before spawning. A crash between these steps can miss an invocation; a crash during a command can leave partial side effects. On scheduler restart, abandoned running records become `interrupted`; Vardr does not automatically retry them. Inspect effects before retrying a job that sends messages, purchases, or changes external systems.

## Lifetimes

The runner uses nonblocking pipes and creates a process group per local command. Escaped descendants cannot keep pipe handling alive beyond the 250 ms cleanup grace. On timeout or normal SIGINT/SIGTERM shutdown, the group is killed and the child reaped. Commands should finish their work within their process lifetime; background daemons are not a supported job type. A machine power loss or SIGKILL can bypass cleanup. The next scheduler startup marks unfinished records for inspection, but does not claim that every escaped child or external operation stopped.

Malformed job files are reported individually and do not block valid jobs. Corrupt history for one job is exposed as a recovery/history error. Startup failures that can still write the data directory leave a bounded last-error record in `service.json`.

T3 dispatch uses bounded HTTP requests. The agent continues under T3's lifetime and approval controls, not Vardr's local process timeout. Its receipt is recorded as `dispatched`; job `timeout_seconds` currently applies to command/CLI execution, not the remote T3 task.

## Existing schedulers

`discover` reads `launchctl list` for the current user. It changes nothing. Existing LaunchAgents, Codex schedules, and T3 schedules stay independent. Adoption and combined inventory are not implemented in this draft.

## Current limits

One user and one host per installation, local filesystem storage, no distributed lock. The dashboard binds only to `127.0.0.1`, rejects unexpected Host headers, and requires a private entry URL (or its strict HttpOnly cookie) for the page and a per-process API token for requests. It is not a remote control server. Multi-machine viewing needs a deliberate authenticated transport in a future version.

Jobs execute as the current user, not inside a Vardr sandbox. Store definitions only from trusted sources. No private machine configuration belongs in this public repository.
