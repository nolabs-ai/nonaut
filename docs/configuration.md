# Configuration

Nonaut keeps all persistent settings in one JSON file:

```text
$XDG_CONFIG_HOME/nonaut/config.json
```

When `XDG_CONFIG_HOME` is unset, the path is
`~/.config/nonaut/config.json` on Linux and macOS. Nonaut creates the file on
first run and preserves it across application updates.

## Defaults

The generated configuration is intentionally conservative:

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
      "read": [],
      "write": []
    },
    "workspaces": {}
  }
}
```

The TUI updates the same file when a provider connects successfully or the
user changes the provider, model, reasoning effort, or theme.

## Workspace example

This example denies operations in unconfigured workspaces, enables the Rust
toolchain for one project, and allows Cargo to reach crates.io:

```json
{
  "ui": {
    "theme": "dark"
  },
  "agent": {
    "provider": "codex",
    "providers": {}
  },
  "tools": {
    "timeout_seconds": 120
  },
  "sandbox": {
    "unconfigured_workspaces": "deny",
    "network": {
      "mode": "blocked",
      "allow_domains": []
    },
    "filesystem": {
      "read": [],
      "write": []
    },
    "workspaces": {
      "~/Work/nonaut": {
        "profiles": ["rust"],
        "network": {
          "mode": "allowlist",
          "allow_domains": [
            "index.crates.io",
            "static.crates.io"
          ]
        },
        "tools": {
          "timeout_seconds": 600
        }
      }
    }
  }
}
```

## Settings

### Agent and UI

- `ui.theme` selects the TUI palette.
- `agent.provider` stores the successfully connected provider ID.
- `agent.providers.<provider>.model` stores the selected provider model.
- `agent.providers.<provider>.reasoning` stores its reasoning effort.
- `agent.providers.<provider>.base_url` overrides an OpenAI-compatible endpoint.
- `agent.providers.<provider>.api_key_env` names the environment variable that
  contains its API key. Secrets are never stored in the configuration file.

For example, a custom OpenAI-compatible service can be configured as:

```json
"agent": {
  "provider": "openai-compatible",
  "providers": {
    "openai-compatible": {
      "base_url": "https://api.example.com/v1",
      "api_key_env": "EXAMPLE_API_KEY",
      "model": "example-model"
    }
  }
}
```

### Tools

`tools.timeout_seconds` limits each tool call, `run_command`, and `/exec`.
The default is 120 seconds and the maximum is 3600. A workspace can override
it under `sandbox.workspaces.<path>.tools`.

When a command times out or is interrupted, Nonaut stops its entire process
group rather than leaving child processes running.

### Filesystem

Tools always receive read/write access to the selected workspace. Additional
host paths can be granted globally or per workspace:

```json
"filesystem": {
  "read": ["~/.config/tool/settings.json"],
  "write": ["~/Work/shared-output"]
}
```

Paths must be absolute or start with `~/`. Workspace filesystem entries are
added to the global entries.

### Network

`sandbox.network.mode` accepts:

- `blocked` — no network access; this is the default.
- `allowlist` — HTTPS through Nonaut's domain-filtering proxy.
- `open` — unrestricted network access from sandboxed tools.

An allowlist requires at least one domain:

```json
"network": {
  "mode": "allowlist",
  "allow_domains": ["api.example.com", "*.packages.example.com"]
}
```

Allowlisted traffic is HTTPS on port 443. `*.` matches subdomains; a bare `*`
is rejected. Cloud metadata and link-local destinations remain denied. A
workspace network object replaces the global network object rather than
extending it.

### Workspace selection

Before each operation, Nonaut canonicalizes its target path and selects the
most specific configured ancestor. This allows one session to work across
multiple projects while applying each project's own rules.

`sandbox.unconfigured_workspaces` controls unmatched operations:

- `allow` keeps them within the session scope using global policy.
- `deny` requires the operation to fall under a configured workspace.

Commands select policy from their working directory. Use either form:

```text
/exec --cwd ../another-project cargo test
```

```sh
nonaut exec --cwd ../another-project -- "cargo test"
```

See [development profiles](profiles.md) for toolchain access and
[sandbox and security](security.md) for the security implications of each
setting.
