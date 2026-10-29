<p align="center">
  <img src="assets/nonaut-hero.png" alt="Nonaut astronaut mascot beside the Nonaut wordmark" width="720">
</p>

<p align="center">
  <strong>A small, security-first coding agent powered by <a href="https://github.com/nolabs-ai/nono">nono</a>.</strong>
  <br>
  <sub>Give your agent a workspace, not your whole machine.</sub>
</p>

Nonaut is a minimal terminal coding agent where provider authentication and
inference stay in the control plane while every model-requested tool runs in a
fresh, kernel-enforced sandbox.

## Features

- Workspace-scoped filesystem access with network blocked by default
- Codex, Claude, Gemini, GitHub Copilot, and OpenAI-compatible providers
- Live in-app review of staged, unstaged, and untracked changes
- Reversible agent turns with `/undo`
- Composable Rust, Python, Node.js, Go, and Bun development profiles
- Persistent sessions that can detach and reattach

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh
```

The installer verifies the release checksum and installs `nonaut` to
`~/.local/bin`. Nonaut embeds Nono, so no separate sandbox installation is
required. The standard build omits control-plane enrollment and audit shipping.
Install the opt-in build when those capabilities are needed:

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh -s -- --control-plane
```

Open a project and start the TUI:

```sh
cd my-project
nonaut
```

On first use, choose a provider and authenticate with its CLI or API-key
environment variable. Nonaut only saves the provider after the connection
succeeds.

## Preview

<p align="center">
  <img src="assets/nonaut-preview.png" alt="Nonaut terminal interface showing workspace access and a blocked sandbox request">
</p>

## Documentation

- [Getting started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Sandbox and security](docs/security.md)
- [Development profiles](docs/profiles.md)
- [Commands and keyboard controls](docs/commands.md)
- [Providers](docs/providers.md)
- [Development and releases](docs/development.md)

See the [documentation index](docs/README.md) for the complete guide.

## Security

Tools receive read/write access to the active workspace and an isolated
per-workspace home. The real home, ambient credentials, and network remain
unavailable unless the user explicitly grants additional capabilities.

The TUI and provider connection run outside the sandbox. See the
[security model](docs/security.md) for boundaries, platform behavior, and
limitations.

## Development

Building from source requires Rust 1.99.1.

```sh
git clone git@github.com:nolabs-ai/nonaut.git
cd nonaut
make ci
make run
```

Release builds include every provider by default. Selective source builds can
use Cargo features to include only the required providers; see
[development and releases](docs/development.md#select-providers).

Contributor commands and the release process are documented in
[docs/development.md](docs/development.md).

## License

Licensed under the [Apache License 2.0](LICENSE).
