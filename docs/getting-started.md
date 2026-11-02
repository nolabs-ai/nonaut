# Getting started

## Requirements

Nonaut supports Linux and macOS on x86-64 and ARM64. Provider requirements are:

- Codex uses the `codex` app server and an existing ChatGPT/Codex login.
- Claude uses the `claude` CLI and a Claude subscription login. An
  `ANTHROPIC_API_KEY` is supported as a fallback.
- Gemini uses `GEMINI_API_KEY`.
- GitHub Copilot uses an installed, authenticated `copilot` CLI.
- OpenRouter uses `OPENROUTER_API_KEY`; Ollama uses its local server by default.
- Custom OpenAI-compatible endpoints are configured in `config.json`.

Nono 0.80 is embedded in Nonaut and does not need to be installed separately.

## Install a release

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh
```

The installer selects the platform binary, verifies its SHA-256 checksum, and
installs it to `~/.local/bin`. Set `NONAUT_INSTALL_DIR` to use another
directory. Run the same command again to update.

The standard build has no enrollment or central audit-delivery dependency.
Install the control-plane build to add `nonaut enroll`, `nonaut unenroll`, and
`nonaut audit`:

```sh
curl -fsSL https://raw.githubusercontent.com/nolabs-ai/nonaut/main/install.sh | sh -s -- --control-plane
```

Remove local enrollment credentials with `nonaut unenroll`. This retains queued
audit data and does not revoke the device in the remote control plane.

Running the standard installer later replaces it with the lean build. The
installer records the selected flavor, so rerunning either command updates the
matching flavor normally.

## Start a project session

```sh
cd path/to/project
nonaut
```

Select a provider on first launch. CLI-backed providers use their existing
login, while API-backed providers read credentials from environment variables.
The provider choice is persisted only after the connection succeeds.

On first launch, Nonaut finds the project root and proposes a minimal policy
with detected development packs, project read/write access and blocked network.
After confirmation, it saves the file under `~/.config/nonaut/projects/`.
Later launches preserve manual changes. The real user home remains isolated.

Edit the generated policy in place and keep its filename unchanged for automatic
loading. Nonaut looks up the exact filename derived from the project directory;
it does not discover other files by their `root` field. To use your own filename,
launch with `nonaut --config /path/to/policy.json`.

Run `nonaut config` to see the active entry and config path. Inside Nonaut,
`/permissions` shows effective access; `/permissions full` includes runtime
grants. Add explicit
network domains, external paths or named references only when the project needs
them.

## Project instructions

Nonaut includes `AGENTS.md` from the workspace root in provider instructions,
falling back to `CLAUDE.md`. It accepts only a regular, single-linked file and
caps it at 32 KiB, preventing a workspace from linking to a file outside the
sandbox.

## Verify the sandbox without a provider

`nonaut exec` uses the same policy-pinned Nono sandbox as agent tools:

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
