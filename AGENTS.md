# Vardr

This is a public MIT tool. Keep personal automation definitions, credentials, logs, and machine-specific state out of this repository.

The CLI, scheduler, and optional UI share the Rust core. Scheduling must work with the UI closed. Keep the idle process small and bound output/history; do not require a browser, Node, or an agent to remain running.

Preserve the distinction between a T3 dispatch receipt and an execution result. Failure investigations are opt-in, separate from the original outcome, and never recursively launch investigations.

Run `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --locked`. Test behavior through real CLI/runner paths. Do not add unused future-trigger abstractions.
