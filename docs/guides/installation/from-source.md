# Install from source

## Prerequisites

- Git.
- A Rust toolchain with Cargo and a native C/C++ build toolchain. Embedded Lua is built from vendored sources.
- Access to a supported remote model endpoint or a running local model server.

The workspace uses Rust edition 2021. It does not declare a minimum supported Rust version in the workspace manifest; use a current stable toolchain rather than assuming an older version is supported.

## Build

From the repository root:

```sh
cargo build --release -p rness-cli
./target/release/rness --help
```

The executable is `target/release/rness`. To install it into Cargo's binary directory instead:

```sh
cargo install --path crates/rness-cli
rness --help
```

Make sure Cargo's binary directory is on your `PATH` if you choose installation.

## Experimental local submission support

Standard builds omit the local control endpoint and `rness send`. To opt in when
installing from this checkout (preserving existing configuration):

```sh
./install.sh --experimental-control
# To replace an existing ~/.local/bin/rness:
./install.sh --experimental-control --replace-binary
```

For a manual build or Cargo installation, enable the feature explicitly:

```sh
cargo build --release -p rness-cli --features experimental-control
cargo install --path crates/rness-cli --features experimental-control
```

Verify the executable you installed with `rness --help` and `rness send --help`.
A listener still starts only when you pass `--control-socket`. See the
[experimental local submission guide](../control-socket.md) for editor/script
usage, durable acceptance versus execution guarantees, recovery and limits.
Windows named-pipe support is implemented but not yet validated on Windows.

## Verify the checkout

```sh
cargo test --workspace
```

The suite includes local scripted-provider and loopback HTTP tests. Passing these tests does not verify your provider credentials, model availability, or a particular gateway's reasoning support.

## Configure a model

Continue with [First configuration and session](../../tutorials/first-configuration.md). rness does not choose a default model for you. Supply a CLI selection or configure a default profile or an agent with a profile.

## Updating

Before updating an existing checkout, inspect `git status` and preserve your local changes. Build and test the desired revision using the commands above. Do not assume that configuration declarations or durable session formats have a published compatibility guarantee; consult [known limitations](../../project/known-limitations.md).

Personal configuration lives outside the checkout. Updating repository examples does not update `~/.rness/init.lua` automatically.
