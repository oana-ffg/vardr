# Signed releases and automatic updates

Vardr uses the standalone-binary update pattern from [Ratatoskr Skill Sync](https://github.com/oana-ffg/ratatoskr-skill-sync), with its own independent signing key.

The official endpoint is `https://github.com/oana-ffg/vardr/releases/latest/download/latest.json`. The trusted public key is committed in `src/update-public-key.txt`. It cannot be replaced by a manifest. Update assets use Tauri/Minisign signatures, verified by `minisign-verify` 0.3. The manifest's version must be newer than the installed semantic version, and the verified executable must report the same version before replacement.

The private signing key is stored only in the repository's `TAURI_SIGNING_PRIVATE_KEY` GitHub Actions secret. It is not stored in this repository or written to a local key file. Release jobs receive it through their environment. Do not print it, pass it as a command argument, or include it in artifacts. GitHub cannot export the secret after storage; maintain access to the repository's Actions configuration. Key rotation for existing installations requires an explicit trust migration.

## Build a release

1. Update `Cargo.toml` and `Cargo.lock` together, review the change, and pass CI.
2. Run **Build signed draft release** from the reviewed commit on `main`.
3. The workflow tests/builds Apple Silicon macOS, Intel macOS, and Linux x64 binaries (built on Ubuntu 22.04). It signs each with the pinned Tauri CLI version, embeds the release version in its trusted signature comment, and verifies the signature against the public key compiled into the binary.
4. The assembly job requires all three artifacts and signatures, writes `latest.json`, and creates a **draft** GitHub Release pinned to the build commit.
5. Inspect/test the candidate artifacts, then publish the draft when ready. Automatic updates see only published stable releases. Do not replace assets under an already-published version.

No Node or Tauri runtime ships with Vardr. Node is used only by the release job's signer. These signatures authenticate updates; they are separate from Apple Developer ID signing/notarization. Publisher signing is not configured in this draft.

## Client behavior

- `vardr update`: check without installing.
- `vardr update --install`: install a newer verified binary, with the service stopped.
- `vardr verify BINARY SIGNATURE`: verify a separately downloaded release asset with the compiled trust key.
- `vardr serve`: automatically check once per local date at/after the configured time, when scheduled workers are idle. Persist the attempt before networking, including failed attempts, to avoid repeated requests.

The updater enforces HTTPS, bounded manifest/binary downloads, a Minisign signature, a newer semantic version, and an executable-version match. Candidate executables are staged next to the installed binary so a noexec system temp directory does not prevent verification. Version output is captured in memory with a 1 KiB limit and a ten-second timeout. Automatic replacement restarts the scheduler using the same arguments and data directory. In-flight scheduled jobs and investigations defer installation. A failed check/install is saved in `updates.json`; schedules continue to run. Until the first stable release exists, a missing manifest is a no-update result.

Forks must deliberately replace the endpoint and compiled trust key, and configure their own signing secret. The official repository's secret cannot be used by forks.
