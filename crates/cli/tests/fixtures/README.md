# Edo integration-test fixtures

This directory doubles as a **machine-runnable integration suite** (via the
`edo-integration-tests` crate in `../`) and a **human-runnable umbrella
monorepo** that a developer can `cd` into to exercise `edo` against real
manifests.

Local source `path`s are written relative to this umbrella directory (matching
the convention in `examples/`), so all manual invocations must be run from
`tests/fixtures/`, not from a subproject directory.

## Build the CLI once

```bash
cargo build -p edo-cli
```

## Run from the umbrella

```bash
cd tests/fixtures
cargo run -p edo-cli -- list
cargo run -p edo-cli -- run //hello_local/emit
cargo run -p edo-cli -- checkout //hello_local/emit /tmp/out
cargo run -p edo-cli -- run //hello_script/build
cargo run -p edo-cli -- run //hello_compose/bundle
cargo run -p edo-cli -- run //cross_project_consumer/final
```

## Error repros

```bash
cd tests/error_fixtures/bad_toml && cargo run -p edo-cli -- list
cd tests/error_fixtures/unresolved_source && cargo run -p edo-cli -- list
```

## Network/container opt-in

```bash
cd tests/net_fixtures/net_git && cargo run -p edo-cli -- run //net_git/build
cd tests/net_fixtures/net_remote && cargo run -p edo-cli -- run //net_remote/build
cd tests/net_fixtures/net_container_script && cargo run -p edo-cli -- run //net_container_script/build
cd tests/net_fixtures/rust_src && cargo run -p edo-cli -- run //rust_src/build
```
