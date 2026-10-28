# Development and releases

## Build from source

Nonaut requires Rust 1.99.1.

```sh
git clone git@github.com:nolabs-ai/nonaut.git
cd nonaut
make run
```

The default build includes every provider and excludes enrollment and audit
shipping. Include the optional control-plane crate with:

```sh
cargo build --features control-plane
```

### Select providers

Official release binaries include all providers. Source builds can exclude the
default set and enable only the required providers:

```sh
cargo build --release --no-default-features \
  --features provider-codex,provider-gemini
```

Available provider features are:

- `provider-codex`
- `provider-claude`
- `provider-gemini`
- `provider-copilot`
- `provider-openrouter`
- `provider-ollama`
- `provider-openai-compatible`

Features compose normally. For example, a selective build can also include
the control plane:

```sh
cargo build --release --no-default-features \
  --features provider-claude,provider-copilot,control-plane
```

To build a release tag, check out that tag before running the same command.
Only compiled providers appear in the provider picker, and dependencies used
solely by omitted providers are not built.

## Development commands

| Command | Action |
| --- | --- |
| `make ci` | Run the complete local CI suite |
| `make run` | Build an optimized binary and launch Nonaut |
| `make build` | Build the optimized binary |
| `make check` | Check formatting, lint, installer syntax, and unit tests |
| `make integration` | Run provider-free sandbox and TUI integration tests |
| `make smoke` | Exercise the release binary through `nonaut exec` |
| `make fmt` | Format the Rust workspace |
| `make lint` | Run Clippy with warnings denied |
| `make test` | Run the Rust test suite |
| `make clean` | Remove Cargo build artifacts |

Run `make ci` before opening a pull request. Integration tests do not connect
to a provider or consume model tokens.

## Install from main

Build and install the current `main` branch with:

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh -s -- --from-main
```

Add `--control-plane` to build and install that flavor from main.

This requires Cargo. Running it again downloads and rebuilds the latest main
branch. The regular installer can replace a main build with a release, and
`--from-main` can replace a release with a development build.

## Releases

Push a version tag matching `Cargo.toml`, such as `v0.0.3`, to publish release
binaries, checksums, and signed build provenance for supported Linux and macOS
targets.
