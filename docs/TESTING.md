# fin-stream tests and benchmarks

```bash
cargo test --lib                    # unit tests inside the modules
cargo test --doc                    # crate docs, and every Rust block in README.md and docs/*.md
cargo test --test '*'               # integration and property tests
cargo run --example tape            # and normalize, feed_health, replay
cargo bench --bench tick_hot_path   # the Criterion numbers in docs/ARCHITECTURE.md
```

CI runs `cargo check`, the unit tests (`cargo test --lib`), the doctests (which include
README.md and docs/*.md), the integration tests and a build of every example. As of 2.11.3
(2026-09-30) all 7,840 unit tests pass; the 17 that failed through 2.11.2 were fixed (seven were
real bugs, ten had wrong expectations; see CHANGELOG.md).
