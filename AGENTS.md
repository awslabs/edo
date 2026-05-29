# Repository Guidelines

## Project Overview

Edo is a Rust build tool for reproducible, environment-controlled builds (OS images, cross-compilation, GLIBC-pinned binaries). It is organized around four pluggable abstractions — **Storage**, **Source**, **Environment**, **Transform** — coordinated by a `Context` (config + plugin registry) and a `Scheduler` (parallel DAG executor). Builtin implementations are registered against a `Context` via `register_core` from the `edo-core` crate.

## Architecture & Data Flow

- `edo-cli` parses CLI args, calls `cmd::create_context` (`crates/cli/src/cmd/mod.rs`) which:
  1. `Context::init(args)`
  2. `register_core(&ctx)` — registers all builtin kinds keyed by string into `Registry` (DashMap-per-trait)
  3. `ctx.add_farm(//default, LocalFarm)` — auto-registers the default local environment
  4. `ctx.load_project(locked)` — walks `edo.toml`, parses to `Node`s, resolves dependencies, optionally applies `edo.lock.json`
- A subcommand (`run`, `checkout`, `update`, `list`, `prune`) drives the resulting `Context`.
- For `run`: `Scheduler::run(ctx, addr)` builds a `daggy::Dag` keyed by `Addr`, then executes three phases — **add** (DAG build + transitive reduction), **fetch** (hash + cache check + `prepare`), **run** (parallel dispatch via `tokio::mpsc` + `Semaphore` of N workers, default 8). Failed/retryable transforms drop into an interactive `dialoguer` prompt (view log / retry / shell / quit).
- `Transform` lifecycle: `prepare → stage → transform`. Output is an `Artifact` (OCI-style manifest of content-addressed BLAKE3 layers) cached in layered `Storage` (`local + source-cache(s) + optional build-cache + optional output-cache`).
- All cross-trait wiring is plugin-style: kind strings (`s3`, `local`, `container`, `git`, `image`, `script`, …) map to `Arc<dyn Handler<T>>` factory closures; trait objects are `Arc`-wrapped via the `arc_handle` proc-macro (declared trait `Foo` becomes `FooImpl`, with a cheap-to-clone newtype `Foo(Arc<dyn FooImpl>)`).

## Key Directories

```
crates/cli/   binary `edo-cli` — clap entry, subcommands in src/cmd/
crates/edo/   library — abstractions, Context, Scheduler, Storage model
crates/core/  library — builtin Storage/Source/Environment/Transform/Vendor impls
tests/        `edo-integration-tests` crate — black-box CLI tests
docs/         design.md, requirements.md, components/{storage,source,environment,transform}.md
```

Inside `crates/edo/src/`:
- `context/` — `Context`, `Addr`, `Node/Data`, `Registry`, `Project` loader, `Schema` (TOML v1), `Lock`, `Log`/`LogManager`, `Handle`
- `storage/` — `Storage`, `Backend` trait, `Artifact`/`Layer`/`Id` (BLAKE3), `LocalBackend`
- `source/` — `Source` trait + `cache()` default method, `Vendor`, resolvo-backed `Resolver`
- `environment/` — `Environment`, `Farm`, VFS helpers
- `transform/` — `Transform`, `TransformStatus`, `transform_err!` macro
- `scheduler/` — `Scheduler`, `Graph`, per-node atomic state, interactive `execute`
- `util/` — BLAKE3 `Reader`/`Writer`, `copy_r`, subprocess helpers

Inside `crates/core/src/`:
- `source/{git,local,oci,remote,vendor}.rs`
- `transform/{import,compose,script,cargo_vendor,go_vendor}.rs`
- `environment/{local,container}.rs`
- `storage/s3/`, `vendor/oci.rs`
- `lib.rs` — `register_core(&Context)` entry

Runtime artefacts (gitignored): `.edo/` (per-project work dir, local cache), `*.lock.json` (the `**.lock.json` glob is broader than `edo.lock.json` — be deliberate when committing files with that suffix).

## Development Commands

The workspace has `default-members = ["crates/cli"]`, so plain commands only touch the CLI binary. Use `--workspace` or `-p <crate>` for the rest.

```bash
# Build the CLI binary (target/debug/edo-cli)
cargo build
cargo run -- run //hello_local/emit              # run a fixture target

# Build / check everything
cargo build --workspace
cargo check --workspace --all-targets

# Lint and format (no project-local rustfmt/clippy config; rely on defaults)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# Supply-chain policy (must pass when adding/upgrading deps)
cargo deny check
```

Test commands — see "Testing & QA" below.

## Code Conventions & Common Patterns

- **Edition / toolchain**: edition 2024 across all crates; resolver 2; no `rust-toolchain.toml`; no `rust-version` is pinned, but edition 2024 implies recent stable Rust.
- **Trait handles**: declare traits with `#[arc_handle]` (`arc-handle` crate). The macro renames the trait to `<Name>Impl` and emits a `Name(Arc<dyn <Name>Impl>)` newtype. Implement `<Name>Impl` for your type, wrap with `<Name>::new(impl)`, and pass the cheap-to-clone handle. Used by `Backend`, `Source`, `Vendor`, `Environment`, `Farm`, `Transform`.
- **Configuration plumbing**: types implement `FromNode` (needs `Context`) or `FromNodeNoContext` (no `Context`), plus `Definable<E,C>` / `DefinableNoContext<E,C>`. For zero-config types use `non_configurable!(Type, ErrorType)` / `non_configurable_no_context!(...)` (defined in `crates/edo/src/context/builder.rs`).
- **Transform error handling**: inside `Transform::transform` use the `transform_err!(expr)` macro (`crates/edo/src/transform/mod.rs`) instead of `?`. It logs and converts `Err` into `TransformStatus::Failed(None, e)`. Returning `?` from `transform` is wrong — callers expect a `TransformStatus`.
- **Error types**: snafu 0.9 throughout. Each subsystem owns a `<Subsystem>Error` enum in a local `error.rs`, exports `<Subsystem>Result<T>`, and uses `#[snafu(transparent)]` to bubble across layers. Plugin escape hatches: `TransformError::Implementation(Box<dyn Error+Send+Sync>)`, `StorageError::Implementation(...)`. `main` is annotated `#[snafu::report]` for pretty stderr chains.
- **Async**: `tokio` (`features = ["full", "parking_lot", "rt-multi-thread"]`), `async-trait` on every plugin trait, `async-recursion` for recursive DAG construction, `tokio_util::sync::CancellationToken` for scheduler cancellation, `tokio::sync::Semaphore` for the worker pool.
- **Concurrency primitives**: `parking_lot::{Mutex, RwLock}` on hot paths, `dashmap::DashMap` for plugin/transform/farm maps, `Arc<Inner>` "facade + inner state" pattern for `Context`, `Scheduler`, `Storage`, `Log`, `LogManager`.
- **Logging**: `#[macro_use] extern crate tracing;` is at the top of `crates/edo/src/lib.rs` and `crates/core/src/lib.rs` — call `info!`/`debug!`/`error!` unqualified inside those crates. Spans use structured fields: `section = "<subsystem>", component = "<kind>"`, with `addr` interpolated. `tracing-indicatif` provides progress bars; `LogManager::acquire()` is the console mutex used to serialize prompts.
- **Addressing**: `Addr::parse("//<project>/<name>")`. Reserved storage slots: `//edo-local-cache`, `//edo-source-cache/<name>`, `//edo-build-cache`, `//edo-output-cache`. The default farm is `//default` (auto-registered, kind `local`).
- **Kind strings**: lowercase, hyphen-separated (`s3`, `local`, `container`, `git`, `remote`, `image`, `vendor`, `import`, `compose`, `script`, `cargo-vendor`, `go-vendor`).
- **Source caching**: prefer `Source::cache(log, storage)` (cache-aware) over the raw `Source::fetch` (always re-pulls).
- **Builders**: `bon` macro is used for typed builders (e.g. `Id::builder()`).
- **Templating**: `handlebars` is used by `ScriptTransform`. Known variables include `{{install-root}}`, `{{build-root}}`, `{{arch}}`, plus any `--arg KEY=VALUE` passed on the CLI. Read `crates/core/src/transform/script.rs` for the full set.
- **No new abstraction layers**: the README/design docs reference Starlark and a "Build Engine" struct — both are alternatives that were rejected; the implementation is TOML + `Context`/`Scheduler`. Trust `crates/edo/src/context/schema.rs` and `tests/fixtures/**/edo.toml`.

## Important Files

| Topic | Path |
|---|---|
| CLI `main` (clap, `#[tokio::main]`, `#[snafu::report]`) | `crates/cli/src/main.rs` |
| Session bootstrap | `crates/cli/src/cmd/mod.rs` (`create_context`) |
| Subcommand dispatch | `crates/cli/src/cmd/{run,checkout,update,list,prune}.rs` |
| Builtin registration | `crates/core/src/lib.rs` (`register_core`) |
| Context / Registry | `crates/edo/src/context/{mod,registry,handle}.rs` |
| TOML schema (v1) | `crates/edo/src/context/schema.rs` |
| Project loader + lockfile | `crates/edo/src/context/builder.rs`, `context/lock.rs` |
| Node / Data / Component | `crates/edo/src/context/node.rs` |
| Scheduler + DAG | `crates/edo/src/scheduler/{mod,graph,execute,node}.rs` |
| Storage model | `crates/edo/src/storage/{mod,backend,artifact,id,local}.rs` |
| Source / resolver | `crates/edo/src/source/{mod,vendor,resolver}.rs` |
| Environment / Farm / VFS | `crates/edo/src/environment/{mod,farm,vfs}.rs` |
| Transform trait + macro | `crates/edo/src/transform/mod.rs` |
| Workspace config | `Cargo.toml`, `deny.toml` |
| Canonical TOML examples | `tests/fixtures/hello_local/edo.toml`, `hello_script/edo.toml`, `hello_compose/edo.toml`, `tests/net_fixtures/net_container_script/edo.toml` |

There is no `examples/` directory on disk despite README references; use `tests/fixtures/` and `tests/net_fixtures/` for canonical configurations. `examples/hello_oci` is referenced as broken in the docs and does not exist.

## Runtime/Tooling Preferences

- **Toolchain**: stable Rust supporting edition 2024 (Rust ≥ 1.85). No `rust-toolchain.toml` is committed; do not add one without team agreement.
- **Package manager**: cargo only. Workspace `[workspace.dependencies]` pins all versions — every crate consumes deps via `{ workspace = true }`. Add new deps to root `Cargo.toml` first, then reference from member crates.
- **Build artefact name**: the CLI binary is `edo-cli` (Cargo default; no `[[bin]]` override). Integration tests resolve it via `assert_cmd::Command::cargo_bin("edo-cli")`.
- **Excluded paths**: `examples/`, `.edo/`, `tests/error_fixtures/`, `tests/fixtures/`, `tests/net_fixtures/` are listed in `Cargo.toml`'s `exclude` so cargo never treats fixtures as packages.
- **Supply chain**: `cargo-deny` is the gate. License allowlist (Apache-2.0/MIT/BSD-2/3-Clause/ISC/MPL-2.0/Zlib/Unicode-3.0/CDLA-Permissive-2.0/BSL-1.0/CC0-1.0/Unlicense/MIT-0/bzip2-1.0.6) is in `deny.toml`; `multiple-versions = "warn"`; only `crates.io` is allowed as a registry. Run `cargo deny check` before adding or upgrading dependencies.
- **Lint / format config**: none committed (`rustfmt.toml`, `clippy.toml`, `[workspace.lints]` all absent). Default `rustfmt --edition 2024` and stock `clippy` apply. Do not add a project-local config without coordinating.
- **No CI / Makefile / justfile** is committed.

## Testing & QA

- Test crate: `tests/` → `edo-integration-tests` (workspace member, `publish = false`). Fixture trees (`tests/fixtures/`, `tests/net_fixtures/`, `tests/error_fixtures/`) are `exclude`d so cargo never compiles them.
- Strategy: black-box CLI tests via `assert_cmd::Command::cargo_bin("edo-cli")`. Each test calls `common::copy_fixture(...)` (or `copy_umbrella()`) to clone the fixture into a `TempDir`, then injects `--storage <tempdir>/.edo-test-store` to isolate state. Helpers live in `tests/src/common/{fixtures,network}.rs`.
- Unit tests live inline in `crates/edo/src/**/*.rs` (`#[cfg(test)] mod tests` in ~14 modules including scheduler, context, storage, util). `crates/core` has no inline tests. Several traits (`Source`, `Vendor`, `Backend`, `Environment`, `Farm`, `Transform`) generate mocks under `#[cfg(test)] use mockall::automock`.
- Network gating is **runtime-only** (no `#[ignore]`, no Cargo features):
  - `network::run_git_source`, `run_remote_source`, `run_cargo_vendor_build`, `run_go_vendor_build` always execute and require live internet.
  - `run_script_in_container` early-returns unless `EDO_TEST_CONTAINER=1` and `podman` or `docker` is on `PATH`.
- `tests/tests/umbrella.rs::manual_smoke` is annotated `#[serial]` (`serial_test`) — it mutates shared filesystem state; do not parallelize it.

Common invocations:

```bash
# All tests (will hit the network — see gating above)
cargo test --workspace

# Unit tests only
cargo test -p edo

# Integration tests, single file
cargo test -p edo-integration-tests --test run
cargo test -p edo-integration-tests --test checkout
cargo test -p edo-integration-tests --test errors
cargo test -p edo-integration-tests --test list
cargo test -p edo-integration-tests --test update_and_lock
cargo test -p edo-integration-tests --test prune
cargo test -p edo-integration-tests --test umbrella

# Network tests (require connectivity)
cargo test -p edo-integration-tests --test network

# Container test (additionally requires podman/docker)
EDO_TEST_CONTAINER=1 cargo test -p edo-integration-tests --test network run_script_in_container
```

When fixing or adding behaviour, write tests at the layer that can actually break: prefer the `crates/edo` unit module that owns the invariant; fall back to a CLI fixture under `tests/fixtures/` when the surface is end-to-end. Do not create mocks; the existing `mockall::automock` setup is sufficient.
