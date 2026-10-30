# Configuration

Nonaut stores persistent settings in `$XDG_CONFIG_HOME/nonaut/config.json`,
which defaults to `~/.config/nonaut/config.json`. The file lives outside every
workspace so an agent cannot widen its own permissions.

## Defaults

The generated configuration is conservative:

```json
{
  "ui": { "theme": "dark" },
  "agent": { "providers": {} },
  "tools": { "timeout_seconds": 120 },
  "workspace": {
    "prompt_on_unconfigured": true,
    "definitions": {}
  },
  "sandbox": {
    "network": { "mode": "blocked", "allow_domains": [] },
    "filesystem": { "read": [], "write": [] }
  }
}
```

With no saved workspace, the exact directory where `nonaut` starts becomes a
one-time read/write workspace. Its parent and siblings are not granted. An
interactive launch offers to save that directory, select a configured
workspace, or continue without changing the configuration.

## Named workspaces

```json
{
  "workspace": {
    "default": "nonaut",
    "prompt_on_unconfigured": true,
    "definitions": {
      "nonaut": {
        "path": "~/Work/nonaut",
        "profiles": ["rust"],
        "references": {
          "nono": { "path": "~/Work/nono" }
        },
        "network": {
          "mode": "allowlist",
          "allow_domains": [
            "github.com",
            "api.github.com",
            "index.crates.io",
            "static.crates.io"
          ]
        },
        "tools": { "timeout_seconds": 600 }
      }
    }
  }
}
```

The primary workspace is read/write. References are read-only and exist only
for that workspace. Configuring another workspace does not make it reachable
from the active session.

Select a workspace explicitly from any directory:

```console
$ nonaut --workspace nonaut
$ nonaut -w nonaut
$ nonaut exec --workspace nonaut -- cargo test
```

Use `--workspace .` to choose the exact current directory even when a default
workspace is configured.

Selection order is:

1. `--workspace NAME` or `--workspace .`;
2. the most specific configured workspace containing the launch directory;
3. `workspace.default`;
4. the exact launch directory as a one-time workspace.

The selected workspace is pinned for the entire session.

## Workspace commands

```console
$ nonaut workspace list
$ nonaut workspace show nonaut
$ nonaut workspace add ~/Work/nonaut --name nonaut --profile rust --default
$ nonaut workspace add ~/Work/nonaut --reference nono=~/Work/nono
$ nonaut setup
$ nonaut workspace edit nonaut
$ nonaut workspace default nonaut
$ nonaut workspace remove nonaut
```

Running interactive Nonaut from an unconfigured directory provides the visual
equivalent of `workspace add`, including profile, network and reference setup.
`nonaut setup` opens it explicitly for the current directory.

## Development profiles

Profiles expose installed toolchains read-only and create private writable
caches and a private workspace HOME. Available profiles are `rust`, `python`,
`node`, `go` and `bun`. Profiles do not enable network access.

## Additional filesystem access

Global grants apply to every workspace:

```json
"sandbox": {
  "filesystem": {
    "read": ["~/.config/tool/settings.json"],
    "write": ["~/Work/shared-output"]
  }
}
```

A workspace may add its own grants with the same `filesystem` object. Paths
must be absolute or begin with `~/`. Workspace grants extend global grants.

## Network

The global `sandbox.network` policy is inherited unless a workspace supplies
its own complete `network` object:

- `blocked` denies network access and is the default.
- `allowlist` permits HTTPS through Nonaut's filtering proxy.
- `open` permits unrestricted network access from sandboxed tools.

An allowlist must contain at least one domain. `*.example.com` matches
subdomains; a bare `*` is rejected. Cloud metadata and link-local destinations
remain denied.

## Tool timeout

`tools.timeout_seconds` applies globally and defaults to 120 seconds. A
workspace can override it under its `tools` object. Values must be between 1
and 3600 seconds. Timed-out or interrupted commands have their complete process
group stopped.

## Agent and UI settings

- `ui.theme` selects the TUI palette.
- `agent.provider` stores the successfully connected provider ID.
- `agent.providers.<provider>.model` stores the selected model.
- `agent.providers.<provider>.reasoning` stores its reasoning effort.
- `base_url` and `api_key_env` configure an OpenAI-compatible provider. The
  environment variable name is stored; its secret value is not.
