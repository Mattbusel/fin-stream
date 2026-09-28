# fin-stream tests and benchmarks

```bash
cargo test --doc                    # crate docs, and every Rust block in README.md and docs/*.md
cargo test --test '*'               # integration and property tests
cargo run --example tape            # and normalize, feed_health, replay
cargo bench --bench tick_hot_path   # the Criterion numbers in docs/ARCHITECTURE.md
```

CI runs `cargo check`, the doctests (which include README.md and docs/*.md), the integration tests
and a build of every example. `cargo test --lib` currently has 17 failing unit tests in
newer analytics modules (alert, cross_asset, fix, lob_sim, order_flow, pattern, risk,
toxicity and others); they are known and tracked separately from the streaming core.
