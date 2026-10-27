# Providers

Nonaut supports Codex and Claude while keeping their authentication and
inference outside the tool sandbox.

## Codex

Codex integration launches `codex app-server`, uses its advertised models and
reasoning levels, and starts the normal Codex login flow when needed.

Requirements:

- The official `codex` CLI available on `PATH`
- An existing ChatGPT/Codex account

Choose **Codex / ChatGPT** during first launch or run `/provider` later.

## Claude

Claude subscription integration launches the official `claude` CLI with its
built-in tools disabled. Claude receives only Nonaut's MCP tools, which execute
through Nono.

Requirements:

- The official `claude` CLI
- A Claude subscription login

If no subscription login is present and `ANTHROPIC_API_KEY` is set, Nonaut can
use the Anthropic API transport instead.

## Persisted settings

Nonaut stores the selected provider only after a successful connection. Model
and reasoning choices are stored per provider in the unified
[configuration file](configuration.md).

Credentials remain owned by the provider CLI or environment. Nonaut does not
copy credentials into its config, sandbox environment, or session metadata.

Use `/provider`, `/model`, and `/reasoning` to change the active settings.

## Project context

Every provider receives the shared Nonaut safety instructions, the workspace
path, and an accepted `AGENTS.md` or `CLAUDE.md` file from the project root.
See [project instructions](getting-started.md#project-instructions) for file
validation rules.
