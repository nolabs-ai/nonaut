<p align="center">
  <img src="assets/nonaut-banner.png" alt="nonaut" width="720">
</p>

<p align="center">
  A security-first agent harness powered by <a href="https://github.com/nolabs-ai/nono">nono</a>.
</p>

> **Other harnesses ask the agent to respect a boundary. Nonaut lets it try.
> The boundary remains technically unavoidable.**
>
> A minimal terminal agent where authentication and inference stay in the
> control plane, while every model-requested tool runs in a fresh,
> kernel-enforced sandbox.

## Usage

Nonaut embeds Nono 0.80, so you do not need to install `nono` separately.

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh
```

The installer selects the correct Linux or macOS binary, verifies its SHA-256
checksum, and installs it to `~/.local/bin`. Run the installer again whenever you want to upgrade to the latest release. Set `NONAUT_INSTALL_DIR` to choose another directory.

```sh
nonaut
```

## Preview

<p align="center">
  <img src="assets/nonaut-preview.png" alt="Nonaut terminal interface showing workspace access and a blocked sandbox request">
</p>

## Security model

- Workspace access is read/write; everything outside it is denied.
- Network access is blocked by default. The [sandbox policy](#configuration)
  can open it or restrict it to an allowlist.
- Provider credentials and ambient environment variables are not inherited.
- Tool results include concise diffs and sandbox denials.

The TUI and provider connections run outside the sandbox; only execution
requested by the model crosses the nono boundary.

## Configuration

Nonaut keeps all persistent settings in
`$XDG_CONFIG_HOME/nonaut/config.json`, defaulting to
`~/.config/nonaut/config.json` on Linux and macOS. It creates the file with
defaults on first run. Successful provider connections and model, reasoning
and theme choices made in the TUI update this same file.

By default tools get read/write access to the workspace, no other paths, and
no network access. The generated first-run configuration is intentionally
conservative:

```json
{
  "ui": {
    "theme": "dark"
  },
  "agent": {
    "providers": {}
  },
  "tools": {
    "timeout_seconds": 120
  },
  "sandbox": {
    "unconfigured_workspaces": "allow",
    "network": {
      "mode": "blocked",
      "allow_domains": []
    },
    "filesystem": {
      "read": []
    },
    "workspaces": {}
  }
}
```

The `sandbox` section can open network access, restrict it to an HTTPS domain
allowlist, or grant additional read-only paths. For example:

```json
{
  "ui": {
    "theme": "dark"
  },
  "agent": {
    "provider": "codex",
    "providers": {
      "codex": {
        "model": "gpt-5.6-luna",
        "reasoning": "low"
      }
    }
  },
  "tools": {
    "timeout_seconds": 600
  },
  "sandbox": {
    "unconfigured_workspaces": "deny",
    "network": {
      "mode": "blocked",
      "allow_domains": []
    },
    "filesystem": {
      "read": ["~/.cargo/registry"]
    },
    "workspaces": {
      "~/Work/nono": {
        "profile": "rust-dev",
        "network": {
          "mode": "allowlist",
          "allow_domains": ["index.crates.io", "static.crates.io", "github.com"]
        }
      }
    }
  }
}
```

- `sandbox.network.mode` is `blocked` (default), `open`, or `allowlist`. Open network lets
  a tool send anything it can read in the workspace anywhere; use `blocked`
  or `allowlist` for code you do not want leaving the machine.
- `allow_domains` lets tools reach those hosts over HTTPS (port 443) through a
  local proxy that checks every connection. `*.` matches subdomains; `*` on
  its own is rejected. Cloud metadata and link-local addresses are always
  denied.
- `sandbox.filesystem.read` paths are readable, never writable.
- `tools.timeout_seconds` (default 120, up to 3600) caps every tool call,
  command and `/exec`; raise it for long builds or test suites. When a call
  times out or is interrupted, everything it started is stopped.
- `sandbox.unconfigured_workspaces` is `allow` (default) or `deny`. With
  `deny`, reads, writes, searches and commands must fall under an explicitly
  configured workspace.
- Workspace selection uses the canonical operation path and the most specific
  configured ancestor. Commands must provide a working directory; use
  `/exec --cwd DIR COMMAND` or `nonaut exec --cwd DIR -- COMMAND`.
- A workspace entry may select the built-in `default` or `rust-dev` profile,
  replace the global network policy, add read-only paths, and override the
  tool timeout. `rust-dev` exposes an installed rustup toolchain without
  inheriting ambient credentials.
- On Linux, `allow_domains` needs Landlock network rules (Linux 6.7+); older
  kernels keep network fully blocked. Landlock limits connections by port
  rather than address, so the proxy's port is reachable on any host; a
  seccomp supervisor would close that gap.

The configuration lives outside the workspace on purpose: tools can write the
workspace, so a policy stored there could be edited to widen the sandbox.

## Project instructions

If the workspace root has an `AGENTS.md` (or, failing that, a `CLAUDE.md`),
nonaut adds it to every provider's instructions so the model learns the
project's build, test and style conventions. Only a regular file is read, up
to 32 KiB; symlinks and hard links are ignored so a tool cannot point it at a
file outside the workspace.

## Undo

Nonaut snapshots the workspace before every agent turn and `/exec`. `/undo`
restores the latest snapshot; run it again to step back further. `.git`,
`target`, `node_modules` and `.gitignore`d files are not tracked. Snapshots
are kept under `$XDG_STATE_HOME/nonaut/undo`, outside the sandbox's reach,
and removed when nonaut exits.

## Providers

Nonaut currently supports two providers:

- Codex / ChatGPT through the `codex` app-server and your existing login.
- Claude through the official `claude` CLI and subscription login, with
  `ANTHROPIC_API_KEY` available as a fallback.

## Commands

Nonaut reuses your existing Codex or Claude login and remembers the selected
provider, model, and reasoning effort.

| Command | Action |
| --- | --- |
| `/provider` | Choose or reconnect a provider |
| `/model` | Select a model |
| `/reasoning` | Select reasoning effort |
| `/new` | Start a fresh session |
| `/exec <command>` | Run a command directly inside nono |
| `/undo` | Restore files to before the last turn |
| `/theme` | Choose the interface theme |
| `/clear` | Clear the screen |
| `/detach` | Keep the session running and return to the shell |
| `/q` | Quit |

`Up` and `Down` recall earlier prompts in this session. `Shift+Enter` inserts a newline. `Esc` interrupts an active response. Sending a
new prompt while streaming interrupts the current response first.

Press `Alt+2` to open the live Review view and inspect the complete working-tree
diff, including staged and untracked files. Press `Alt+1`, `Esc`, or `q` to return
to the agent. The view refreshes automatically; use the arrow keys or
`PageUp`/`PageDown` to scroll, `Tab`/`Shift+Tab` to move between changed files,
and `/` to search.

`/exec` does not require a connected provider. It runs through the same fresh,
workspace-only nono sandbox used for agent tool calls, which makes policy and
denial testing immediate:

```text
/exec ls .
/exec ls ~/.ssh
/exec printf 'hello\n' > test.txt
```

The same path is available without opening the TUI, which is useful for scripts
and policy checks:

```sh
nonaut exec -- "ls ."
nonaut exec -- "ls ~/.ssh"
```

Detached sessions keep their active provider and conversation in a local
worker. They are created only when `/detach` is used and expire after 24 hours
while detached.

```sh
nonaut ps
nonaut attach <session>
nonaut kill <session>
nonaut kill --all # -a also works
```

Session metadata follows XDG and is stored under
`$XDG_STATE_HOME/nonaut/sessions`, defaulting to
`~/.local/state/nonaut/sessions`. Provider credentials are never copied into
session metadata.

## Development

Building from source requires Rust 1.99.1.

```sh
git clone git@github.com:nolabs-ai/nonaut.git
cd nonaut
make run
```

Development commands:

| Command | Action |
| --- | --- |
| `make ci` | Run the complete local CI suite |
| `make run` | Build an optimized binary and launch Nonaut |
| `make build` | Build the optimized binary |
| `make check` | Check formatting, run Clippy, and run unit tests |
| `make script-check` | Validate the installer script syntax |
| `make integration` | Run provider-free tests against the real sandbox and TUI |
| `make smoke` | Smoke-test the release binary through `nonaut exec` |
| `make fmt` | Format the Rust workspace |
| `make lint` | Run Clippy with warnings denied |
| `make test` | Run the Rust test suite |
| `make clean` | Remove Cargo build artifacts |

Before opening a pull request, run:

```sh
make ci
```

The integration suite does not connect to a provider or consume model tokens.

### Install from main

Developers can build and install the latest `main` branch directly. This
requires Cargo and Rust 1.99.1.

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh -s -- --from-main
```

Running the command again downloads and rebuilds the latest source.
Switching between the regular installer and `--from-main` replaces the existing
binary with the selected release or development channel.

## Limitations

- Claude subscription access requires the official Claude Code CLI.
- Nonaut does not restore conversations after restart. Provider CLIs may retain
  their own local session data.

## Releases

Push a version tag matching `Cargo.toml`, such as `v0.0.2`, to publish signed
build provenance, checksums, and binaries for Linux and macOS on x86_64 and
ARM64.

## License

Licensed under the [Apache License 2.0](LICENSE).
