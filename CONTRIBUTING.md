# Contributing to fin-stream

Thank you for your interest in contributing! This document explains how to get
started, run tests, and submit changes.

## Development Environment Setup

1. Install Rust via [rustup](https://rustup.rs/) (stable toolchain, 1.75+).
2. Clone the repository:
   ```sh
   git clone https://github.com/Mattbusel/fin-stream
   cd fin-stream
   ```
3. Build the project:
   ```sh
   cargo build
   ```

## Running Tests

```sh
cargo test
```

To also run benchmarks:

```sh
cargo bench
```

All tests must pass before submitting a pull request.

## Coding Standards

- Format code with `cargo fmt` before committing.
- Lint with `cargo clippy -- -D warnings`; resolve all warnings.
- Avoid `unwrap()` and `expect()` in production paths; use proper error
  handling with `?` or explicit matching.
- Async code must not block the executor; use `tokio::task::spawn_blocking`
  for CPU-intensive work.
- Keep unsafe blocks to an absolute minimum and document every one.
- Public items must have doc comments (`///`).

## Branch and PR Workflow

1. Fork the repository and create a feature branch from `main`:
   ```sh
   git checkout -b feat/your-feature-name
   ```
2. Make your changes, ensuring `cargo fmt`, `cargo clippy`, and `cargo test`
   all pass locally.
3. Push your branch and open a Pull Request against `main`.
4. Fill in the PR template, linking any related issues.
5. At least one maintainer review is required before merging.
6. Squash or rebase commits to keep history clean.

## Reporting Bugs

Please open an issue on GitHub with:

- A clear, descriptive title.
- Steps to reproduce the problem.
- Expected behavior vs. actual behavior.
- Rust version (`rustc --version`) and operating system.
- Any relevant logs or stack traces.

## Commit Message Convention

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add WebSocket market data adapter
fix: handle back-pressure in stream buffer
docs: document reconnect behavior
```

## License

By contributing you agree that your contributions will be licensed under the
same license as this project.
