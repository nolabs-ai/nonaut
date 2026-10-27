# Getting started

## Requirements

Nonaut supports Linux and macOS on x86-64 and ARM64. A provider also requires
its official command-line client:

- Codex uses the `codex` app server and an existing ChatGPT/Codex login.
- Claude uses the `claude` CLI and a Claude subscription login. An
  `ANTHROPIC_API_KEY` is supported as a fallback.

Nono 0.80 is embedded in Nonaut and does not need to be installed separately.

## Install a release

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh
```

The installer selects the platform binary, verifies its SHA-256 checksum, and
installs it to `~/.local/bin`. Set `NONAUT_INSTALL_DIR` to use another
directory. Run the same command again to update.

## Start a project session

```sh
cd path/to/project
nonaut
```

Select a provider on first launch. If authentication is needed, Nonaut starts
the provider's normal login flow. The provider choice is persisted only after
the connection succeeds.

Nonaut grants tools read/write access to the starting workspace, blocks
network access, and hides the real user home by default. Use a development
[profile](profiles.md) and explicit [workspace policy](configuration.md) when
the project needs a toolchain or network domains.

## Project instructions

Nonaut includes `AGENTS.md` from the workspace root in provider instructions,
falling back to `CLAUDE.md`. It accepts only a regular, single-linked file and
caps it at 32 KiB, preventing a workspace from linking to a file outside the
sandbox.

## Verify the sandbox without a provider

`nonaut exec` uses the same policy and fresh Nono sandbox as agent tools:

```sh
nonaut exec -- "pwd"
nonaut exec -- "printf 'hello\n' > hello.txt"
nonaut exec -- "cat ~/.ssh/id_rsa"
```

The first two commands operate in the workspace. The final command is denied
under the default policy.

Continue with [commands and keyboard controls](commands.md), then configure
project-specific access in [configuration](configuration.md).

## Current limitations

Nonaut does not restore a conversation after its process or detached worker
exits. A detached session can preserve a live conversation for up to 24 hours,
but it is not durable conversation storage.
