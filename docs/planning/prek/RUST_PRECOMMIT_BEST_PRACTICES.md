# Pre-commit Framework Best Practices for Rust Projects

Comprehensive guide on implementing pre-commit hooks for Rust projects with focus on linters, formatters, performance optimization, and CI/CD integration.

**Last Updated:** April 2026

---

## Table of Contents

1. [Pre-commit Framework Overview](#pre-commit-framework-overview)
2. [Essential Rust Linters & Formatters](#essential-rust-linters--formatters)
3. [Hook Chain Strategy](#hook-chain-strategy)
4. [Performance Optimization](#performance-optimization)
5. [Configuration Pattern](#configuration-pattern)
6. [Stage-Specific Hooks](#stage-specific-hooks)
7. [CI/CD Integration](#cicd-integration)
8. [Troubleshooting & Tips](#troubleshooting--tips)

---

## Pre-commit Framework Overview

### What is pre-commit?

**Pre-commit** (from https://pre-commit.com) is a multi-language package manager for pre-commit hooks. Key characteristics:

- **Language agnostic**: Written in Python but can run hooks in any language
- **Automatic environment management**: Manages hook installations without requiring root access
- **Isolation**: Each hook runs in its own isolated environment
- **Distributed**: Share hooks across projects via git repositories

### Why Use Pre-commit for Rust?

1. **Consistency**: Ensures all developers run the same checks
2. **Integration**: Single configuration file for all linting/formatting tools
3. **Performance**: Built-in caching and parallelization
4. **Flexibility**: Per-stage hooks (pre-commit, pre-push, commit-msg, etc.)
5. **CI/CD Friendly**: Easy integration with GitHub Actions, GitLab CI, etc.

---

## Essential Rust Linters & Formatters

### Priority Tier 1: Always Include

#### 1. **Clippy** (Linter)

The official Rust linter. 800+ lints across 10 categories.

- **Official**: Part of Rust ecosystem
- **Coverage**: Correctness, performance, style, complexity
- **Auto-fix**: Limited support with `--fix`
- **Installation**: Included with Rustup

```yaml
# In .pre-commit-config.yaml
- repo: local
  hooks:
  - id: clippy
    name: Clippy
    entry: cargo clippy
    language: system
    pass_filenames: false
    stages: [pre-commit, pre-push]
    files: '\.rs$'
```

**Clippy Configuration** (`clippy.toml`):

```toml
# Set MSRV for Rust version-specific lints
msrv = "1.70"

# Configure specific lints
too-many-arguments-threshold = 8
type-complexity-threshold = 500

# Custom deny list
disallowed-names = ["foo", "bar", "baz"]
```

**Key Categories**:
- `clippy::correctness` (deny) - Code is outright wrong
- `clippy::suspicious` (warn) - Most likely wrong
- `clippy::style` (warn) - Non-idiomatic code
- `clippy::complexity` (warn) - Overly complex code
- `clippy::perf` (warn) - Suboptimal performance
- `clippy::pedantic` (allow) - Strict, occasional false positives

**Recommended Args**:
```yaml
args: ['--all-targets', '--all-features', '--', '-W', 'clippy::all']
```

---

#### 2. **Rustfmt** (Formatter)

Official code formatter for Rust. **Non-negotiable**.

- **Official**: Rust core tool
- **Deterministic**: Same output every time
- **Coverage**: All Rust code
- **No false positives**: Only formatting, no lint categories

```yaml
# In .pre-commit-config.yaml
- repo: local
  hooks:
  - id: rustfmt
    name: Rustfmt
    entry: cargo fmt
    language: system
    pass_filenames: false
    stages: [pre-commit]
    files: '\.rs$'
    args: ['--all', '--check']  # --check to fail on uncommitted formatting
```

**Rustfmt Configuration** (`rustfmt.toml`):

```toml
edition = "2021"
max_width = 100
hard_tabs = false
tab_spaces = 4
newline_style = "Auto"
use_small_heuristics = "Default"
reorder_imports = true
reorder_modules = true
remove_nested_parens = true
format_code_in_doc_comments = true
normalize_comments = true
wrap_comments = true
comment_width = 100
```

---

### Priority Tier 2: Highly Recommended

#### 3. **Cargo Audit** (Security)

Audits Cargo.lock against RustSec advisory database.

- **Purpose**: Detect known vulnerabilities in dependencies
- **Speed**: ~500ms typically
- **Database**: Automatically updated or cached

```yaml
- repo: https://github.com/rustsec/cargo-audit
  rev: v0.18.3
  hooks:
  - id: cargo-audit
    name: Cargo Audit
    entry: cargo audit
    language: system
    pass_filenames: false
    stages: [pre-commit, pre-push]
    args: ['--deny', 'warnings']
```

#### 4. **Cargo Deny** (Dependencies)

Comprehensive dependency linting tool.

- **Checks**: Advisories, bans, licenses, sources
- **License compliance**: Verify acceptable licenses
- **Ban duplicates**: Detect multiple versions
- **Speed**: ~1-2 seconds

```yaml
- repo: https://github.com/EmbarkStudios/cargo-deny
  rev: 0.14.16
  hooks:
  - id: cargo-deny
    name: Cargo Deny
    entry: cargo deny check
    language: system
    pass_filenames: false
    stages: [pre-commit, pre-push]
    args: ['--all-features']
```

**Cargo Deny Configuration** (`deny.toml`):

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
    "Unicode-3.0",
]
deny = [
    "GPL-2.0",
    "GPL-3.0",
    "AGPL-3.0",
]

[bans]
multiple-versions = "deny"
wildcards = "deny"
allow = []
skip = []

[sources]
allow-org = { github = ["mycompany"] }
```

---

#### 5. **Typos** (Spell Check)

Catches common spelling mistakes in code and comments.

```yaml
- repo: https://github.com/crate-ci/typos
  rev: v1.16.0
  hooks:
  - id: typos
    name: Typos
    args: ['--config', '.typos.toml']
```

**Typos Configuration** (`.typos.toml`):

```toml
[default]
locale = "en-us"

# Ignore specific words
ignore-words = ["nd", "ths"]

# Custom patterns
[default.extend-ignore-identifiers]
# Allow Rust-specific identifiers
```

---

### Priority Tier 3: Optional Enhancements

#### 6. **Taplo** (TOML Formatter)

Formats Cargo.toml and other TOML files.

```yaml
- repo: https://github.com/tamasfe/taplo
  rev: 0.9.1
  hooks:
  - id: taplo-format
    name: Taplo Format
    args: ['format', '--option', 'indent_string="  "']
    files: '\.toml$'
```

#### 7. **Toml Lint**

Lints TOML files for common issues.

```yaml
- repo: https://github.com/crate-ci/toml-lint
  rev: v0.1.1
  hooks:
  - id: toml-lint
    name: Toml Lint
    files: '\.toml$'
```

#### 8. **Check JSON** (Pre-commit Hooks)

Validates JSON syntax.

```yaml
- repo: https://github.com/pre-commit/pre-commit-hooks
  rev: v4.5.0
  hooks:
  - id: check-json
    name: Check JSON
  - id: check-yaml
    name: Check YAML
  - id: check-toml
    name: Check TOML
  - id: end-of-file-fixer
    name: Fix End of File
  - id: trailing-whitespace
    name: Trim Trailing Whitespace
```

---

## Hook Chain Strategy

### Recommended Execution Order

```
1. Fast, Non-Blocking (< 100ms each)
   ├─ Trailing whitespace
   ├─ End-of-file-fixer
   ├─ Check YAML/JSON/TOML
   └─ Typos

2. Format (Order matters)
   ├─ Rustfmt (must run before Clippy)
   └─ Taplo (for TOML)

3. Analysis (CPU intensive)
   ├─ Clippy
   ├─ Cargo Audit
   └─ Cargo Deny

4. Final Checks
   └─ Custom validation scripts
```

### Blocking vs Non-Blocking Strategy

| Hook | Blocking | Rationale |
|------|----------|-----------|
| Rustfmt | YES | Code style must be consistent |
| Clippy | YES | Catches real bugs |
| Cargo Audit | YES | Blocks security issues |
| Cargo Deny | YES | Enforces policy (licenses, versions) |
| Typos | NO | Can be fixed post-commit |
| Trailing whitespace | YES | Cleanliness |

### Stage Strategy

| Stage | Timing | Best For | Example |
|-------|--------|----------|---------|
| `pre-commit` | Before commit recorded | Quick, safe checks | Format, lint, whitespace |
| `pre-push` | Before push to remote | Integration checks | Full test suite, heavy analysis |
| `commit-msg` | After commit message written | Message validation | Conventional commits |
| `post-commit` | After commit succeeded | Notifications | Slack alerts, caching |

---

## Performance Optimization

### 1. Caching Strategy

Pre-commit caches hooks by default in `~/.cache/pre-commit/`.

**GitHub Actions Cache Example**:

```yaml
- name: Set Python version
  run: echo "PY=$(python -VV | sha256sum | cut -d' ' -f1)" >> $GITHUB_ENV

- uses: actions/cache@v3
  with:
    path: ~/.cache/pre-commit
    key: pre-commit|${{ env.PY }}|${{ hashFiles('.pre-commit-config.yaml') }}
```

**GitLab CI Cache Example**:

```yaml
cache:
  key: pre-commit-${CI_COMMIT_REF_SLUG}
  paths:
    - .cache/pre-commit
  policy: pull-push
```

### 2. Cargo Cache Management

**Problem**: Clippy rebuilds incrementally every time.

**Solution**: Cache Cargo build artifacts.

```yaml
# GitHub Actions
- uses: Swatinem/rust-cache@v2
  with:
    cache-on-failure: true

# GitLab CI
cache:
  paths:
    - target/
    - .cargo/
  key: ${CI_COMMIT_REF_SLUG}
```

### 3. File-Type Filtering

Run hooks only on relevant files:

```yaml
hooks:
- id: clippy
  files: '\.rs$'  # Only .rs files
  exclude: '^tests/fixtures/'  # Exclude test fixtures

- id: taplo-format
  files: '\.toml$'  # Only TOML files
```

### 4. Parallel Execution

Pre-commit runs hooks in parallel by default on multi-core systems:

```yaml
# Force sequential execution for specific hooks
- id: clippy
  require_serial: true  # If Clippy causes issues in parallel
```

### 5. Skip Expensive Hooks During Development

Use environment variable to skip heavy checks:

```bash
# Skip audit during development
SKIP=cargo-audit,cargo-deny git commit -m "WIP"
```

### 6. Conditional Hook Execution

Run expensive hooks only on certain branches:

```python
# .pre-commit-config.yaml with conditional stages
- repo: https://github.com/EmbarkStudios/cargo-deny
  rev: 0.14.16
  hooks:
  - id: cargo-deny
    stages: [pre-push]  # Only on push, not every commit
```

### 7. Benchmarking Your Setup

**Profile hook execution**:

```bash
time pre-commit run --all-files

# Output:
# real    0m5.234s
# user    0m15.892s
# sys     0m1.234s
```

**Track individual hook times**:

```bash
pre-commit run --all-files --verbose
```

### 8. Optimization Checklist

- [ ] Enable `pre-commit.ci` for CI/CD
- [ ] Use `language: system` for local Rust tools (vs downloading)
- [ ] Cache Cargo artifacts in CI
- [ ] Filter files by extension (`.rs`, `.toml`, etc.)
- [ ] Use `pre-push` stage for heavy checks (20+ seconds)
- [ ] Mark heavy hooks with `require_serial: false` for parallelization
- [ ] Exclude test fixtures and generated files

---

## Configuration Pattern

### Recommended .pre-commit-config.yaml

```yaml
# .pre-commit-config.yaml
# Core configuration for pre-commit hooks

# Minimum required pre-commit version
minimum_pre_commit_version: '2.15.0'

# Default behavior
fail_fast: false  # Continue checking other hooks on failure
exclude: '\.lock$|^target/'  # Global exclusions

# Default stages if not specified per hook
default_stages: [pre-commit]

repos:
  # ============================================================================
  # TIER 1: BASIC FILE CHECKS (Fast, < 100ms)
  # ============================================================================
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
        name: Trim trailing whitespace
        stages: [pre-commit, pre-push]

      - id: end-of-file-fixer
        name: Fix end of file
        stages: [pre-commit, pre-push]

      - id: check-yaml
        name: Check YAML syntax
        args: ['--unsafe']

      - id: check-json
        name: Check JSON syntax

      - id: check-toml
        name: Check TOML syntax

      - id: check-case-conflict
        name: Check for case conflicts

  # ============================================================================
  # TIER 1.5: SPELL CHECK (Optional but recommended)
  # ============================================================================
  - repo: https://github.com/crate-ci/typos
    rev: v1.16.0
    hooks:
      - id: typos
        name: Spell check
        args: ['--config=.typos.toml']
        stages: [pre-push]  # Optional: Skip on every commit

  # ============================================================================
  # TIER 2: CODE FORMATTING (Must run before linting)
  # ============================================================================
  - repo: local
    hooks:
      # Rustfmt: Official Rust formatter
      - id: rustfmt
        name: Format Rust code
        entry: cargo fmt
        language: system
        pass_filenames: false
        stages: [pre-commit]
        files: '\.rs$'
        args: ['--all', '--check']

      # Taplo: TOML formatter
      - id: taplo-format
        name: Format TOML
        entry: taplo format
        language: system
        files: '\.toml$'
        stages: [pre-commit]
        args: ['--option', 'indent_string="  "']
        pass_filenames: true

  # ============================================================================
  # TIER 3: LINTING & ANALYSIS (CPU intensive)
  # ============================================================================
  # Clippy: Official Rust linter
  - repo: local
    hooks:
      - id: clippy
        name: Clippy linter
        entry: cargo clippy
        language: system
        pass_filenames: false
        stages: [pre-commit, pre-push]
        files: '\.rs$'
        args:
          - '--all-targets'
          - '--all-features'
          - '--'
          - '-W'
          - 'clippy::all'
          - '-W'
          - 'clippy::pedantic'
          - '-D'
          - 'clippy::correctness'

  # ============================================================================
  # TIER 4: SECURITY & DEPENDENCY CHECKS
  # ============================================================================
  # Cargo Audit: Vulnerability scanning
  - repo: https://github.com/rustsec/cargo-audit
    rev: v0.18.3
    hooks:
      - id: cargo-audit
        name: Cargo audit (security check)
        entry: cargo audit
        language: system
        pass_filenames: false
        stages: [pre-push]  # Skip during development
        args: ['--deny', 'warnings']

  # Cargo Deny: Comprehensive dependency check
  - repo: https://github.com/EmbarkStudios/cargo-deny
    rev: 0.14.16
    hooks:
      - id: cargo-deny
        name: Cargo deny
        entry: cargo deny check
        language: system
        pass_filenames: false
        stages: [pre-push]  # Heavy, run on push
        args: ['--all-features']
```

### Complete Clippy Configuration (clippy.toml)

```toml
# Minimum supported Rust version
msrv = "1.70"

# Cognitive complexity (function too complex)
cognitive-complexity-threshold = 30

# Function parameter count
too-many-arguments-threshold = 7

# Type complexity
type-complexity-threshold = 450

# Doc comment style
doc-valid-idents = [
    "HTTP",
    "HTTPS",
    "OAuth",
    "OpenGL",
    "WebGL",
    "JSON",
    "YAML",
    "TOML",
    "UUID",
    "SQL",
    "SQLite",
    "PostgreSQL",
    "MongoDB",
]

# Disabled lints (if needed)
allow-expect-in-tests = true
allow-unwrap-in-tests = true

# Enum variant naming
enum-variant-name-threshold = 3

# Single char lifetime allowed
single-char-lifetime-names-threshold = 5
```

### Complete Cargo Deny Configuration (deny.toml)

```toml
[advisories]
vulnerability = "deny"
unmaintained = "warn"
unsound = "warn"
notice = "warn"
ignore = [
    # Example: CVE-2025-XXXX = ["1.0.0"]
]

[licenses]
allow = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "MIT OR Apache-2.0",
    "ISC",
    "Unicode-3.0",
    "Unicode-DFS-2016",
]
deny = [
    "GPL-2.0",
    "GPL-3.0",
    "AGPL-3.0",
]
copyleft = "warn"
allow-osi-fsf-free = "both"
default = "deny"
confidence-threshold = 0.8

[bans]
# Deny multiple versions
multiple-versions = "deny"
# Deny wildcards
wildcards = "deny"
# List of explicitly allowed duplicate crates
allow = [
    # { name = "ansi_term", version = "=0.11.0" },
]
skip = [
    # { name = "ansi_term", version = "=0.11.0" },
]
skip-tree = []

[sources]
allow-org = { github = ["your-company"] }
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```

### Rust Formatter Configuration (rustfmt.toml)

```toml
# Edition
edition = "2021"

# Formatting options
max_width = 100
hard_tabs = false
tab_spaces = 4
newline_style = "Auto"
indent_style = "Block"

# Spacing
space_before_colon = false
space_after_colon = true
spaces_around_ranges = false

# Imports
reorder_imports = true
reorder_modules = true
use_try_shorthand = true
use_field_init_shorthand = true

# Function formatting
fn_single_line = false
where_single_line = false

# Match blocks
match_block_trailing_comma = true

# Comments
normalize_comments = true
wrap_comments = true
comment_width = 100
format_code_in_doc_comments = true

# Other
remove_nested_parens = true
merge_derives = true
```

### Typos Configuration (.typos.toml)

```toml
[default]
locale = "en-us"

[default.extend-ignore-identifiers]
# Rust identifiers that shouldn't be flagged
```

---

## Stage-Specific Hooks

### Pre-commit Stage (Before commit is recorded)

Used for: Quick checks that should never fail a commit.

```yaml
stages: [pre-commit]
```

**Hooks**:
- Formatting (rustfmt, taplo)
- Trailing whitespace
- Basic syntax checks

**Example**:
```yaml
- id: rustfmt
  stages: [pre-commit]  # Run on every commit
```

### Pre-push Stage (Before push to remote)

Used for: Slower checks that don't block development.

```yaml
stages: [pre-push]
```

**Hooks**:
- Full compilation checks
- Cargo audit / Cargo deny
- Integration tests

**Example**:
```bash
pre-commit install --hook-type pre-push
```

### Commit-msg Stage (After commit message written)

Used for: Validate commit message format.

```yaml
stages: [commit-msg]
```

**Example**: Enforce Conventional Commits

```yaml
- repo: https://github.com/compilerla/conventional-pre-commit
  rev: v2.3.0
  hooks:
  - id: conventional-pre-commit
    stages: [commit-msg]
    args: [--force-single-commit]
```

### Post-commit Stage (After commit succeeds)

Used for: Non-blocking notifications or caching.

```yaml
stages: [post-commit]
```

### Prepare-commit-msg Stage (Before editor opens)

Used for: Auto-populate commit template.

```yaml
stages: [prepare-commit-msg]
```

---

## CI/CD Integration

### GitHub Actions Setup

```yaml
# .github/workflows/pre-commit.yml
name: Pre-commit

on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main, develop]

jobs:
  pre-commit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Set up Python
        uses: actions/setup-python@v4
        with:
          python-version: '3.11'

      - name: Set up Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache Rust
        uses: Swatinem/rust-cache@v2

      - name: Set Python version for cache
        run: echo "PY=$(python -VV | sha256sum | cut -d' ' -f1)" >> $GITHUB_ENV

      - name: Cache pre-commit
        uses: actions/cache@v3
        with:
          path: ~/.cache/pre-commit
          key: pre-commit|${{ env.PY }}|${{ hashFiles('.pre-commit-config.yaml') }}

      - name: Run pre-commit
        uses: pre-commit/action@v3
        env:
          SKIP: cargo-audit  # Optional: skip slow hooks in CI
```

### GitLab CI Setup

```yaml
# .gitlab-ci.yml
pre-commit:
  stage: lint
  image: rust:latest
  before_script:
    - apt-get update && apt-get install -y python3 python3-pip
    - pip install pre-commit
    - rustup component add rustfmt clippy
  cache:
    key: pre-commit-${CI_COMMIT_REF_SLUG}
    paths:
      - .cache/pre-commit
      - target/
  script:
    - pre-commit run --all-files
```

### CircleCI Setup

```yaml
# .circleci/config.yml
jobs:
  lint:
    docker:
      - image: rust:latest
    steps:
      - checkout
      - run:
          name: Install pre-commit
          command: apt-get update && apt-get install -y python3-pip && pip install pre-commit
      - restore_cache:
          keys:
            - v1-pc-cache-{{ checksum ".pre-commit-config.yaml" }}
      - run:
          name: Run pre-commit
          command: pre-commit run --all-files
      - save_cache:
          key: v1-pc-cache-{{ checksum ".pre-commit-config.yaml" }}
          paths:
            - ~/.cache/pre-commit
```

### AWS CodeBuild Setup

```yaml
# buildspec.yml
version: 0.2

phases:
  install:
    runtime-versions:
      python: 3.11
      rust: latest
    commands:
      - pip install pre-commit
      - rustup component add rustfmt clippy

  build:
    commands:
      - pre-commit run --all-files

cache:
  paths:
    - /root/.cache/pre-commit/**
    - target/**
```

---

## Troubleshooting & Tips

### Common Issues

#### 1. "Tool not found" on CI

**Problem**: Cargo tools not available.

**Solution**:
```yaml
- run: rustup component add clippy rustfmt
- run: cargo install cargo-audit cargo-deny
```

Or use `language: system` with pre-installed tools.

#### 2. Slow Clippy on First Run

**Problem**: Clippy rebuilds all dependencies.

**Solution**: Use Cargo cache in CI:

```yaml
- uses: Swatinem/rust-cache@v2
```

#### 3. "pre-commit not installed at .git/hooks"

**Problem**: Hook not properly installed.

**Solution**:
```bash
pre-commit uninstall
pre-commit install
```

#### 4. Conflicting Formatting Issues

**Problem**: Rustfmt and another tool fight.

**Solution**: Ensure proper execution order in `.pre-commit-config.yaml`. Rustfmt should run **before** Clippy.

#### 5. False Positives in Clippy

**Problem**: Clippy reports incorrect violations.

**Solution**: 
```rust
// Suppress specific lint for a block
#[allow(clippy::lint_name)]
fn my_function() {}
```

Or disable in `clippy.toml`:
```toml
# Globally disable specific lints
# clippy-lint-name = "allow"
```

### Performance Tips

1. **Skip pre-commit in CI**: Use `SKIP=cargo-audit,cargo-deny` for non-critical checks
2. **Use pre-push stage**: Move heavy checks (20+ seconds) to pre-push
3. **Enable caching**: Always cache Cargo and pre-commit artifacts
4. **Filter files**: Use `files` to skip unnecessary hook execution
5. **Parallel execution**: Let pre-commit run hooks in parallel (default)
6. **Use mutable caching**: In CI, allow cache updates between runs

### Example Development Workflow

```bash
# Initial setup
git clone <repo>
cd <repo>
pre-commit install
pre-commit install --hook-type pre-push

# Day-to-day development
git commit -m "Feature X"  # Runs pre-commit hooks automatically

# Skip hooks if truly needed (rare)
SKIP=cargo-deny git commit -m "WIP"

# Manual validation before push
pre-commit run --all-files

# Push to remote (runs pre-push hooks)
git push
```

### Maintenance

```bash
# Update all hooks to latest versions
pre-commit autoupdate

# Run all hooks on all files
pre-commit run --all-files

# Clean up cached hook environments
pre-commit clean
pre-commit gc

# Validate configuration
pre-commit validate-config
```

---

## Summary Table

| Tool | Type | Purpose | Speed | Install |
|------|------|---------|-------|---------|
| **Rustfmt** | Formatter | Code style | ~1-2s | System |
| **Clippy** | Linter | Code quality | ~5-10s | System |
| **Cargo Audit** | Security | Vulnerabilities | ~0.5s | External |
| **Cargo Deny** | Policy | Dependencies | ~1-2s | External |
| **Taplo** | Formatter | TOML files | ~0.2s | External |
| **Typos** | Spellcheck | Documentation | ~0.3s | External |

---

## References

- [Pre-commit Documentation](https://pre-commit.com)
- [Clippy Book](https://doc.rust-lang.org/clippy/)
- [Rustfmt Documentation](https://rust-lang.github.io/rustfmt/)
- [Cargo Deny Book](https://embarkstudios.github.io/cargo-deny/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)

