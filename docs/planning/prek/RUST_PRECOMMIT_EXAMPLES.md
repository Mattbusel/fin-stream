# Ready-to-Use Pre-commit Configuration Examples for Rust

Quick-start configurations for different project types and CI/CD platforms.

---

## Quick-Start: Minimal Setup (for small projects)

### .pre-commit-config.yaml

```yaml
minimum_pre_commit_version: '2.15.0'
fail_fast: false
exclude: '\.lock$|^target/'
default_stages: [pre-commit]

repos:
  # Basic file checks
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
      - id: end-of-file-fixer
      - id: check-yaml
      - id: check-json
      - id: check-toml

  # Rust formatting and linting (local Cargo.toml required)
  - repo: local
    hooks:
      - id: rustfmt
        name: Rustfmt
        entry: cargo fmt --all --check
        language: system
        pass_filenames: false
        files: '\.rs$'

      - id: clippy
        name: Clippy
        entry: cargo clippy --all-targets --all-features --
        language: system
        pass_filenames: false
        files: '\.rs$'
        args: ['-W', 'clippy::all', '-D', 'clippy::correctness']
```

### Installation
```bash
pre-commit install
```

---

## Production Setup: Comprehensive Configuration

### .pre-commit-config.yaml

```yaml
minimum_pre_commit_version: '2.15.0'
fail_fast: false
exclude: '\.lock$|^target/|\.git/'
default_stages: [pre-commit]

repos:
  # =========================================================================
  # STAGE 1: File checks (< 100ms each)
  # =========================================================================
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
        name: Trim trailing whitespace
        stages: [pre-commit, pre-push]

      - id: end-of-file-fixer
        name: Fix end of file

      - id: check-yaml
        name: Check YAML
        args: ['--unsafe']

      - id: check-json
        name: Check JSON

      - id: check-toml
        name: Check TOML

      - id: check-case-conflict
        name: Check case conflicts

  # =========================================================================
  # STAGE 2: Spell check (optional)
  # =========================================================================
  - repo: https://github.com/crate-ci/typos
    rev: v1.16.0
    hooks:
      - id: typos
        name: Check spelling
        stages: [pre-push]

  # =========================================================================
  # STAGE 3: Format code (must run before linting!)
  # =========================================================================
  - repo: local
    hooks:
      - id: rustfmt
        name: Format Rust code
        entry: cargo fmt --all
        language: system
        pass_filenames: false
        files: '\.rs$'
        stages: [pre-commit]

      - id: taplo-format
        name: Format TOML
        entry: taplo format
        language: system
        pass_filenames: true
        files: '\.toml$'
        stages: [pre-commit]
        require_serial: false

  # =========================================================================
  # STAGE 4: Lint code (CPU intensive)
  # =========================================================================
  - repo: local
    hooks:
      - id: clippy
        name: Clippy linter
        entry: cargo clippy --all-targets --all-features
        language: system
        pass_filenames: false
        files: '\.rs$'
        stages: [pre-commit, pre-push]
        args:
          - '--'
          - '-W'
          - 'clippy::all'
          - '-W'
          - 'clippy::pedantic'
          - '-D'
          - 'clippy::correctness'

  # =========================================================================
  # STAGE 5: Security checks (heavy, run on pre-push)
  # =========================================================================
  - repo: https://github.com/rustsec/cargo-audit
    rev: v0.18.3
    hooks:
      - id: cargo-audit
        name: Cargo audit
        entry: cargo audit --deny warnings
        language: system
        pass_filenames: false
        stages: [pre-push]

  - repo: https://github.com/EmbarkStudios/cargo-deny
    rev: 0.14.16
    hooks:
      - id: cargo-deny
        name: Cargo deny
        entry: cargo deny check --all-features
        language: system
        pass_filenames: false
        stages: [pre-push]

  # =========================================================================
  # STAGE 6: Conventional commits (optional)
  # =========================================================================
  - repo: https://github.com/compilerla/conventional-pre-commit
    rev: v2.3.0
    hooks:
      - id: conventional-pre-commit
        name: Check conventional commit
        stages: [commit-msg]
```

### clippy.toml

```toml
msrv = "1.70"
cognitive-complexity-threshold = 30
too-many-arguments-threshold = 7
type-complexity-threshold = 450

doc-valid-idents = [
    "HTTP", "HTTPS", "OAuth", "JSON", "YAML", "TOML",
    "UUID", "SQL", "SQLite", "PostgreSQL", "MongoDB",
]

allow-expect-in-tests = true
allow-unwrap-in-tests = true
```

### rustfmt.toml

```toml
edition = "2021"
max_width = 100
hard_tabs = false
tab_spaces = 4
reorder_imports = true
reorder_modules = true
wrap_comments = true
comment_width = 100
format_code_in_doc_comments = true
```

### deny.toml

```toml
[advisories]
vulnerability = "deny"
unmaintained = "warn"
unsound = "warn"
notice = "warn"

[licenses]
allow = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "MIT OR Apache-2.0",
    "ISC",
]
deny = ["GPL-2.0", "GPL-3.0", "AGPL-3.0"]

[bans]
multiple-versions = "deny"
wildcards = "deny"

[sources]
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
```

---

## GitHub Actions CI/CD Integration

### .github/workflows/pre-commit.yml

```yaml
name: Pre-commit Linting

on:
  push:
    branches: [main, develop, 'release/**']
  pull_request:
    branches: [main, develop]

jobs:
  pre-commit:
    runs-on: ubuntu-latest
    timeout-minutes: 30

    steps:
      - name: Checkout code
        uses: actions/checkout@v3
        with:
          fetch-depth: 0

      - name: Set up Python
        uses: actions/setup-python@v4
        with:
          python-version: '3.11'

      - name: Set up Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Generate Python cache key
        run: echo "PY=$(python -VV | sha256sum | cut -d' ' -f1)" >> $GITHUB_ENV

      - name: Cache pre-commit environments
        uses: actions/cache@v3
        with:
          path: ~/.cache/pre-commit
          key: pre-commit|${{ env.PY }}|${{ hashFiles('.pre-commit-config.yaml') }}
          restore-keys: |
            pre-commit|${{ env.PY }}|

      - name: Cache Rust artifacts
        uses: Swatinem/rust-cache@v2
        with:
          cache-on-failure: true
          key: rust-${{ runner.os }}

      - name: Install pre-commit
        run: pip install pre-commit

      - name: Run pre-commit on all files
        run: pre-commit run --all-files
        env:
          SKIP: cargo-audit,cargo-deny  # Optional: skip slow checks initially
```

### .github/workflows/security-checks.yml

```yaml
name: Security Checks (Heavy)

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  security:
    runs-on: ubuntu-latest
    timeout-minutes: 60

    steps:
      - uses: actions/checkout@v3

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2

      - name: Install cargo-audit
        run: cargo install cargo-audit

      - name: Run cargo audit
        run: cargo audit --deny warnings

      - name: Run cargo deny
        run: cargo deny check
```

---

## GitLab CI Integration

### .gitlab-ci.yml

```yaml
stages:
  - lint
  - test
  - security

variables:
  RUST_BACKTRACE: "full"
  CARGO_TERM_COLOR: "always"

lint:pre-commit:
  stage: lint
  image: rust:latest
  before_script:
    - apt-get update && apt-get install -y python3-pip
    - pip install pre-commit
    - rustup component add rustfmt clippy
  cache:
    key: pre-commit-${CI_COMMIT_REF_SLUG}
    paths:
      - .cache/pre-commit
      - target/
      - .cargo/
    policy: pull-push
  script:
    - pre-commit run --all-files
  artifacts:
    reports:
      sast: gl-sast-report.json
    when: always

security:cargo-audit:
  stage: security
  image: rust:latest
  cache:
    key: cargo-${CI_COMMIT_REF_SLUG}
    paths:
      - target/
      - .cargo/
    policy: pull-push
  script:
    - cargo install cargo-audit
    - cargo audit --deny warnings
  only:
    - main
    - merge_requests
```

---

## Docker Setup (for reproducible CI)

### Dockerfile.precommit

```dockerfile
FROM rust:latest

# Install Python and pre-commit
RUN apt-get update && \
    apt-get install -y python3-pip && \
    pip install pre-commit && \
    rm -rf /var/lib/apt/lists/*

# Install Rust components
RUN rustup component add rustfmt clippy

# Install additional tools
RUN cargo install cargo-audit cargo-deny taplo-cli

WORKDIR /workspace

ENTRYPOINT ["pre-commit"]
CMD ["run", "--all-files"]
```

### Usage
```bash
docker build -f Dockerfile.precommit -t rust-precommit .
docker run -v $(pwd):/workspace rust-precommit
```

---

## Development-Only Setup (Skip Heavy Checks)

### .pre-commit-config.yaml (dev-friendly)

```yaml
minimum_pre_commit_version: '2.15.0'
fail_fast: false
exclude: '\.lock$|^target/'
default_stages: [pre-commit]

repos:
  # Fast checks only
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
      - id: end-of-file-fixer
      - id: check-toml
      - id: check-json

  - repo: local
    hooks:
      - id: rustfmt
        name: Rustfmt
        entry: cargo fmt --all --check
        language: system
        pass_filenames: false
        files: '\.rs$'

      # Skip Clippy in pre-commit, run manually or in CI only
      # Users can force with: FORCE_CLIPPY=1 git commit
```

### Makefile helpers

```makefile
.PHONY: pre-commit
pre-commit:
	pre-commit run --all-files

.PHONY: pre-commit-force
pre-commit-force:
	FORCE_CLIPPY=1 pre-commit run --all-files

.PHONY: pre-commit-install
pre-commit-install:
	pre-commit install && \
	pre-commit install --hook-type pre-push

.PHONY: pre-commit-update
pre-commit-update:
	pre-commit autoupdate

.PHONY: lint
lint: pre-commit
	cargo test --all-features

.PHONY: audit
audit:
	cargo audit --deny warnings && \
	cargo deny check
```

---

## Monorepo Setup (Multiple Crates)

### .pre-commit-config.yaml

```yaml
minimum_pre_commit_version: '2.15.0'
fail_fast: false
exclude: '\.lock$|^target/'

repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
      - id: end-of-file-fixer
      - id: check-toml

  - repo: local
    hooks:
      - id: rustfmt
        name: Format all Rust code
        entry: cargo fmt --all
        language: system
        pass_filenames: false
        files: '\.rs$'

      - id: clippy
        name: Lint all workspace
        entry: cargo clippy --all --all-features
        language: system
        pass_filenames: false
        files: '\.rs$'
        args: ['--', '-W', 'clippy::all', '-D', 'clippy::correctness']

      - id: clippy-tests
        name: Lint all tests
        entry: cargo clippy --all --all-features --tests
        language: system
        pass_filenames: false
        files: '\.rs$'
        stages: [pre-push]
        args: ['--', '-W', 'clippy::all']
```

---

## Emergency Bypass

For when you absolutely need to skip checks:

```bash
# Skip specific hooks
SKIP=cargo-audit,cargo-deny git commit -m "Emergency fix"

# Skip all pre-commit hooks (dangerous!)
git commit --no-verify -m "Emergency hotfix"

# For pre-push, use --no-verify
git push --no-verify
```

---

## Implementation Checklist

- [ ] Copy `.pre-commit-config.yaml` to repo root
- [ ] Copy `clippy.toml`, `rustfmt.toml`, `deny.toml` to repo root
- [ ] Run `pre-commit install`
- [ ] Run `pre-commit run --all-files` to verify
- [ ] Add CI/CD workflow (GitHub Actions, GitLab CI, etc.)
- [ ] Update `README.md` with setup instructions
- [ ] Test with `SKIP=cargo-deny git commit`
- [ ] Test with `pre-commit install --hook-type pre-push`
- [ ] Document in `CONTRIBUTING.md`
- [ ] Optional: Add to Makefile for easy access

---

## Helpful Commands

```bash
# Initial setup
pre-commit install
pre-commit install --hook-type pre-push

# Run on all files (useful after adding config)
pre-commit run --all-files

# Run specific hook
pre-commit run rustfmt --all-files

# Update all hooks to latest
pre-commit autoupdate

# Skip a hook for one commit
SKIP=cargo-audit git commit -m "WIP"

# Verify config is valid
pre-commit validate-config

# Uninstall hooks
pre-commit uninstall
pre-commit uninstall --hook-type pre-push

# Troubleshoot: clear cache
pre-commit clean
pre-commit gc
```

