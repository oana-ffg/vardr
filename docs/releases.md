# Signed releases and automatic updates

Vardr has its own existing Tauri/Minisign update key. The compiled trust root in `src/update-public-key.txt` and the desktop configuration stays unchanged. `latest.json` describes three standalone CLIs; `desktop-latest.json` describes the two Mac app archives. Both manifests bind the same reviewed source version. Signatures authenticate the payload and release version; executable version checks also protect standalone installation.

## Credential boundary

Apple Developer ID signing and notarization run only on the local publisher Mac. Keep the certificate private key and App Store Connect API key in encrypted gopass. Supply their entry names, the public DER certificate and public team ID as command options; never commit private credentials or machine profiles. The notary entry is a JSON document with `private_key` (PKCS#8 PEM), `key_id` and `issuer_id`. Existing team credentials may cover multiple apps; Vardr does not require another Apple identity or API key.

The pre-existing Vardr updater key currently exists only as the repository's `TAURI_SIGNING_PRIVATE_KEY` Actions secret, with an empty password. No gopass recovery copy is available. Do not delete or replace it: installed clients trust its public key, and GitHub cannot export the secret. This release flow preserves that key and lets Actions add update signatures only after local Apple signing. A main-only `release` environment gates both workflows. Set the nonsecret `APPLE_TEAM_ID` verification variable in that environment. The existing updater secret remains repository-scoped; the environment gates its signing job, but does not narrow the secret's repository scope. A future recovery/migration must preserve the trust root and verify the destination before removing the old copy.

Actions never receive an Apple private key, certificate bundle, Keychain password or notarization API key. The updater key is exposed only to the signing step, after dependencies and the verifier have been built. Signing failures suppress credential-bearing tool output. Private values must never enter source, caches, logs or distributables.

## Build and finish a release

1. Keep the CLI, desktop Cargo packages, Tauri config and build-tool package versions equal; refresh lockfiles. Open a PR, pass CI and review before merging. The next prepared version is **0.5.0**. It moves macOS installations to the `com.firefungames.vardr` identity, which 0.4.x installations cannot update to automatically; see the one-time migration below.
2. Dispatch **Build release candidates** from the reviewed `main` commit. It tests/builds Apple Silicon and Intel app candidates plus a Linux x64 CLI on Ubuntu 22.04, without signing secrets. Candidate ZIPs strip resource metadata and are validated before extraction.
3. On Brunnr or another trusted publisher Mac, check out that clean exact `origin/main` commit. Full Xcode, macOS 27+, Python 3.11+, OpenSSL, gopass, Cargo and `gh` are required. Run `python3 -B scripts/macos_release.py --help`. Supply the successful candidate run ID, certificate, gopass entry names, team and a new output directory. The tool refuses reused tags/releases, a dirty checkout or a run from another commit/branch/event.
4. The local tool signs both embedded helpers as `com.firefungames.vardr.cli` and both apps as `com.firefungames.vardr`, with Developer ID, hardened runtime and a secure timestamp. It closes the temporary signing Keychain before notarization waits. Each app must receive `Accepted`, then be stapled and pass Gatekeeper. Python packs metadata-free updater archives and rechecks extracted app/helper signatures and tickets. The identical standalone CLI is covered by the app's notary ticket and is separately checked with `codesign --check-notarization`. DMGs contain those stapled apps; their `com.firefungames.vardr.installer` signatures, separate notarization and stapled tickets are verified too.
5. `--draft` creates an **incomplete unpublished draft** containing seven Apple-verified/candidate files, then dispatches **Sign verified update artifacts** with their exact SHA256 hashes and original candidate run ID. That workflow independently checks the draft flag/target commit, Linux candidate provenance, Mac architecture/version, matching standalone/bundled CLI, Apple publisher signatures, notarization and staples before accessing the old updater key. It signs only the five updater payloads, verifies signatures and authenticated versions against the compiled trust root, and attaches the signatures and two manifests. It cannot publish a release.
6. The local tool waits for that exact main dispatch, downloads the draft, verifies the original file hashes and all update signatures/manifests, and requires exactly **14 final files**: three CLIs and their signatures, two app archives and their signatures, two DMGs and two manifests. Keychains, credential images, candidate ZIPs, hashes and diagnostic receipts stay outside draft assets. A failure can leave an incomplete draft; inspect it before retrying rather than publishing it.
7. Test the actual app, standalone CLI, older-install updates, service/permission continuity, native Intel and Linux execution and clean-Mac first launch. Publish only after owner approval. Automatic updates see only published stable releases. Do not replace assets under a published version or publish while signing is running.

The local certificate is matched to its gopass private key in memory, then passed through an encrypted PKCS#12 into a temporary encrypted Keychain. Password-bearing Keychain commands use stdin. Notarytool rejects pipes, so its regular key file lives only in an owner-only temporary AES-256 disk image, excluded from indexing/backups. The file is removed before detaching the image; owned-volume detach gets a forced retry, and cleanup failures retain the original error. SIGTERM/SIGHUP trigger cleanup. SIGKILL/power loss cannot, so inspect and remove only the interrupted run's containers before retrying. Unlocked containers are accessible to processes running as the same user; use a trusted local Mac.

## macOS permission identity and migration

macOS privacy grants may be tied to an executable's designated signing requirement. An ad-hoc requirement can contain a build-specific cdhash. The standalone updater checks both directions: the candidate must satisfy the installed requirement, and the installed executable must satisfy the candidate requirement. Desktop updates do the same for the outer app and bundled CLI after validating the authenticated archive without accepting links, special files or AppleDouble metadata. A changed identity stops replacement and records an error, preserving the installed binary/app. Normal unsigned development builds remain possible, but cannot pass publisher release checks.

### One-time 0.5.0 identity migration

Vardr 0.5.0 uses the app identifier `com.firefungames.vardr`, CLI signing identifier `com.firefungames.vardr.cli`, DMG signing identifier `com.firefungames.vardr.installer`, LaunchAgent `com.firefungames.vardr.runner` and data directory `~/Library/Application Support/com.firefungames.vardr`. Earlier releases used `dev.vardr.desktop`, `dev.vardr.cli`, `dev.vardr.installer`, `dev.vardr.runner` and `~/Library/Application Support/dev.vardr.vardr`. Linux keeps `~/.local/share/vardr`.

The signing identifier is part of the designated requirement. 0.4.x apps, scheduler helpers and standalone CLIs therefore refuse the 0.5.0 update and keep running; the app shows the error in its dashboard. This is intended. The guard stays strict because the new identity loses existing privacy grants either way, so the change must be visible. Upgrade once by hand:

1. Wait until no scheduled job is running or due within a minute, then quit the old Vardr app.
2. Install Vardr 0.5.0 from the DMG and open it. Choose **Move**, or run `/Applications/Vardr.app/Contents/MacOS/vardr migrate`. An installation with its own `--home`/`VARDR_HOME` keeps that directory; run `vardr --home DIR migrate` to replace its scheduler.
3. Grant Full Disk Access again to the scheduler helper `~/Library/Application Support/com.firefungames.vardr/bin/vardr`, and to Vardr.app if it had access before. Approve the Automation prompts that jobs raise. Remove the stale old entries in System Settings by hand. macOS also reports the new background item.
4. Verify permissions with an actual scheduler-owned job.

The migration moves the whole data directory: job definitions, run histories, schedules, settings, connected hosts, update history, ownership receipts and the helper. It refuses before changing anything in these cases:
- the old LaunchAgent was not written from this data directory, or was edited since;
- a job or investigation is running;
- an enabled job is due within the next minute.

Otherwise it stops `dev.vardr.runner` and waits up to that minute for the scheduler to exit, so two schedulers never run at once. A pre-0.5.0 scheduler cannot be paused, so a run it reserved in the instant before these checks is recorded as interrupted, as with any scheduler stop. Such a worker can also fail before recording its run, leaving the failure only in the stopped scheduler's last status report, which the new scheduler overwrites. The migration therefore keeps every error that report lists under a job's id, verbatim, in `migration-failures.json` in the data directory, and names those jobs in its result. Their last scheduled runs may not have happened. Nothing is inferred or replayed; check the jobs' effects before retrying. The migration then holds every Vardr lock and refuses if another command still uses the data. If a step fails before the data moves, the old agent is restarted.

A record written before the move makes the rest resumable. Data is swapped atomically with a short note at the old path, so pre-0.5.0 processes still using that path fail instead of recreating an empty installation. The new `com.firefungames.vardr.runner` starts with the new helper and the old agent's `PATH`, and only then is the old LaunchAgent removed. If replacing the scheduler fails, `vardr migrate` and the app say so; running it again finishes the remaining steps. Logging in before then reloads the old agent, so a retry first stops it the same way.

Until migration, a 0.5.0 app or CLI that finds only old data refuses to start an empty installation, including over a connected Mac's SSH bridge. It also refuses the old default directory passed as `--home`, `VARDR_HOME` or a connected host's remote home; update such host settings to the new directory. If both directories exist, it stops until one is removed. While a move runs, the app defers quitting.

Ad-hoc 0.4.2 installations migrate the same way. Do not restore legacy scheduling owners or reset privacy databases. A signed artifact or successful isolated probe does not establish that Health/iCloud permissions work on another Mac.

## Client behavior

- `vardr update` checks without installing; `vardr update --install` requires the scheduler stopped.
- `vardr verify BINARY SIGNATURE --app-version VERSION` checks the compiled trust key and authenticated version without opening private Vardr state.
- `vardr serve` checks at most once per local date at/after the configured time, only when scheduled workers and investigations are idle. It preserves attempts/errors in `updates.json`; update failures do not stop scheduling.

Standalone downloads require HTTPS, size limits, a newer version, the update signature and an executable-version match. Candidates are staged next to the installed binary to avoid noexec temporary directories, with bounded version-output/timeouts. The bundled CLI updates with its app. The LaunchAgent runs a stable `bin/vardr` copy under the private data directory, independently of the app; app replacement cannot remove the scheduler executable. Reinstallation preserves a newer helper.

The desktop checks once per local date while open, records errors in `desktop-updates.json`, and installs for its next launch without restarting the UI or scheduler. Manual checks bypass daily cadence. Closing exits the app, with confirmation if manual work is active; exit is refused during bundle replacement. Updates keep the data directory and scheduler label; only the one-time 0.5.0 migration changes them.

Forks must deliberately replace endpoints and the compiled trust root and configure their own updater secret. The official repository's secret is unavailable to forks.

## v0.4.1 security patch

Update `rustls-webpki` to 0.103.16 and `webpki-roots` / `webpki-root-certs` to 1.0.10. CLI and core HTTPS clients now reject the compromised eMudhra intermediate CA keys. The native macOS desktop updater uses the operating system's certificate verification. Signed-update verification, schedules, and local automation data are unchanged.
