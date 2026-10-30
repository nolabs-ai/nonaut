# Providers

Nonaut supports CLI-backed providers and OpenAI-compatible APIs while keeping
authentication and inference outside the tool sandbox. Models receive only
Nonaut's tools; every operation still executes through Nono.

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

## Gemini

Gemini uses Google's OpenAI-compatible API. Export `GEMINI_API_KEY`, then
choose **Gemini / Google**. Nonaut discovers the available models and does not
store the key.

## GitHub Copilot

Copilot uses GitHub's official SDK and an installed `copilot` CLI. Sign in with
the CLI first, then choose **GitHub Copilot**. Copilot's built-in tools are
disabled; only Nonaut's confined tools are exposed.

## OpenAI-compatible APIs

Nonaut includes presets for:

- **OpenRouter**, authenticated with `OPENROUTER_API_KEY`.
- **Ollama**, using `http://127.0.0.1:11434/v1` by default. Set
  `OLLAMA_API_KEY` only when the endpoint requires it.
- **OpenAI-compatible API**, for any service that implements model listing and
  chat completions with streaming function calls.

Configure a custom endpoint in `config.json`:

```json
"agent": {
  "providers": {
    "openai-compatible": {
      "base_url": "https://api.example.com/v1",
      "api_key_env": "EXAMPLE_API_KEY",
      "model": "example-model"
    }
  }
}
```

`base_url` and `api_key_env` can also override the Gemini, OpenRouter, and
Ollama presets. The configuration stores only an environment-variable name,
never the credential itself.

## Startup behavior

Nonaut caches provider model catalogs for up to one hour under
`$XDG_CACHE_HOME/nonaut/providers`, defaulting to
`~/.cache/nonaut/providers`. Cache keys separate endpoints and credentials
without storing credential values. Codex and Claude CLI authentication are
still checked when connecting.

Provider sessions and threads are prewarmed in the background. Ordinary prompts
remain in the editor until the provider is fully connected; local slash
commands remain available during setup.

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

Direct API providers pin the rendered system prompt and tool catalog when the
session starts, preserving a stable prompt-cache prefix even if workspace
instructions change later. When direct-provider history reaches its bounded
context threshold, the same provider summarizes it into a standalone
continuation state before proceeding. CLI-backed Codex and Claude integrations
delegate context management to their native harnesses.
