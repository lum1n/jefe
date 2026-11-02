# Working on jefe

- `jefe-core` does no I/O: no clock, no randomness, no files, no processes. Time comes in as `now_ms`; everything the host must do goes out as an `Effect`. CI builds it for `wasm32-unknown-unknown` to keep it that way.
- acc is pinned by git rev in the workspace `Cargo.toml`. Keep acc calls in `jefe-core/src/worker.rs`, `jefe-core/src/merge.rs` and `jefe/src/brain.rs`, so a bump touches few files.
- The brain proposes, the machine disposes: a brain picks one action from the allowed set and `rules::validate` checks it. Never give a brain a way to run commands.
- Avoid new dependencies. No chrono, tracing, async-trait or dirs.
- Before handing off: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Live tests against signed-in harnesses are `#[ignore]`d and need `JEFE_LIVE=1`.
