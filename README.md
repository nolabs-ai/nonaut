<p align="center">
  <img src="assets/nonaut-hero.png" alt="Nonaut astronaut mascot beside the Nonaut wordmark" width="720">
</p>

<p align="center">
  <strong>A small, security-first coding agent powered by <a href="https://github.com/nolabs-ai/nono">nono</a>.</strong>
  <br>
  <sub>Give your agent a workspace, not your whole machine.</sub>
</p>

---

## Why Nonaut?

- **Enforced by Nono.** Every model-requested tool runs inside [Nono](https://github.com/nolabs-ai/nono), a kernel-enforced security boundary built on Seatbelt for macOS and Landlock for Linux.
- **Safe by default.** Agents see the active workspace while your real home, ambient credentials, and network stay unavailable unless explicitly granted.
- **Private control plane.** Provider authentication and model inference remain outside the tool sandbox.
- **Bring your provider.** Use Codex, Claude, Gemini, GitHub Copilot, or any OpenAI-compatible endpoint.
- **Stay in control.** Review staged, unstaged, and untracked changes in the TUI, or roll back a whole agent turn with `/undo`.
- **Built for real projects.** Compose Rust, Python, Node.js, Go, and Bun development profiles, then detach from persistent sessions and reconnect later.

## Preview

<p align="center">
  <img src="assets/nonaut-preview.png" alt="Nonaut terminal interface showing workspace access and a blocked sandbox request">
</p>

## Quick start

Nonaut supports Linux and macOS on x86-64 and ARM64. Install the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh
```

The installer verifies the release checksum and installs `nonaut` to
`~/.local/bin`. Nonaut embeds Nono, so no separate sandbox installation is required.

Open a project and start the TUI:

```sh
cd my-project
nonaut
```

On first use, choose a provider and authenticate with its CLI or API-key
environment variable. Nonaut only saves the provider after the connection
succeeds.

<details>
<summary>Install the optional control-plane build</summary>

The standard build omits control-plane enrollment and audit shipping. Install the opt-in build when those capabilities are needed:

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh -s -- --control-plane
```

</details>

## Security model

| Component | Runs where | Access |
| --- | --- | --- |
| TUI and provider connection | Nonaut control plane | Authentication and model APIs |
| Model-requested tools | Fresh Nono sandbox | Active workspace and an isolated home |
| Additional capabilities | Nono sandbox | Only paths and network explicitly granted |

The TUI and provider connection remain outside the tool sandbox. See the [security model](docs/security.md) for boundaries, platform behavior, and limitations.

## Documentation

- [Getting started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Sandbox and security](docs/security.md)
- [Development profiles](docs/profiles.md)
- [Commands and keyboard controls](docs/commands.md)
- [Providers](docs/providers.md)
- [Development and releases](docs/development.md)

See the [documentation index](docs/README.md) for the complete guide.

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
