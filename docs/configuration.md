# Configuration

Nonaut separates application settings from project permissions. It does not load
configuration from a repository automatically.

## Application settings

`$XDG_CONFIG_HOME/nonaut/config.json` (normally `~/.config/nonaut/config.json`)
holds provider, model, UI and default tool timeout settings:

```json
{
  "agent": {
    "provider": "codex",
    "providers": {
      "codex": { "model": "your-model", "reasoning": "low" }
    }
  }
}
```

The theme defaults to dark and the tool timeout to 120 seconds. Override them
with `ui.theme` and `tools.timeout_seconds`. Project permissions are never
stored in this file. Unknown keys are errors; there is no legacy migration.

## First launch and saved projects

Launch `nonaut` inside a project. Nonaut finds the nearest project marker:
`.git`, `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`,
`requirements.txt`, or a Bun lockfile. Discovery stops at the user's home.
Without a marker it uses the launch directory. Home and filesystem roots are
rejected.

The first interactive launch shows a minimal proposed policy and asks for
confirmation. It saves the policy under
`~/.config/nonaut/projects/<name>-<path-hash>.json`, then starts the session.
Headless execution creates the same minimal policy without an interactive prompt.
Later launches reuse the file without overwriting manual edits.

```json
{
  "name": "my-service",
  "root": "/Users/me/Work/my-service",
  "packs": ["rust"],
  "filesystem": { "readwrite": ["."] },
  "network": { "mode": "blocked" }
}
```

Empty lists and optional sections are omitted. Detection selects only development
packs. External references, credentials, and network permissions require manual
configuration.

Nonaut canonicalizes the project root, resolving symlinks. The filename uses a
SHA-256-derived identifier for that absolute path, so same-named projects do not
collide. The name is a display label. Moving a project changes its saved-policy
lookup; its previous policy is not silently rebound.

Run `nonaut config` to print the resolved project configuration and source path.
This command does not create a project policy.

### Editing or supplying an automatically loaded policy

The recommended workflow is to launch `nonaut` once, accept the minimal policy,
then edit that saved file in place. Later launches load your edits. Keep its
filename unchanged; you do not need to calculate an identifier yourself.

Without `--config`, Nonaut looks for exactly one filename in
`$XDG_CONFIG_HOME/nonaut/projects/`:

```text
<directory-name>-<path-hash>.json
```

If you provision this file yourself, both parts must follow the generated naming
rule:

- `directory-name` is the canonical project directory's basename with only ASCII
  letters, digits, `-` and `_` retained, or `project` if nothing remains.
- `path-hash` is the first 16 bytes of SHA-256 of the canonical absolute path
  bytes, encoded as 32 lowercase hexadecimal characters.

Nonaut does not scan this directory for matching `root` fields. A differently
named policy is ignored by automatic lookup, even if its `root` matches.
Changing the policy's display `name` does not change its lookup filename.
If the expected file is absent, Nonaut offers to create it on interactive launch.
The existing file must pass validation; an invalid policy produces an error
instead of being replaced.

The status panel may abbreviate the hash as `<id>`. This is a display placeholder,
not a literal filename. Use `nonaut config` or `/permissions` to get the full path.
For an arbitrary filename or location, use `--config` as shown below.

## Supplied policies

```sh
cd ~/Work/my-service
nonaut --config ~/policies/rust.json
nonaut exec --config ~/policies/rust.json -- "cargo test"
nonaut config --config ~/policies/rust.json
```

An explicitly supplied policy replaces the saved project policy. It does not
merge with it, create a saved project entry, or redirect to another project.

Without `root`, the file targets the discovered project. With `root`, Nonaut
canonicalizes the declared directory and requires an exact match. Launching
from a project subdirectory still targets the discovered root.

## Filesystem and references

`filesystem.read` grants read-only access and `filesystem.readwrite` grants
both reading and writing. These grants must resolve within the project.
There is no implicit project grant: an empty filesystem section grants no
project access.

`filesystem.deny` blocks reading and writing a literal path and its descendants,
including paths supplied by references or packs. Wildcard patterns are rejected.

```json
{
  "filesystem": {
    "readwrite": ["."],
    "deny": ["./credentials.env"]
  },
  "references": {
    "nono": { "path": "../nono", "access": "read" },
    "cargo-credentials": {
      "path": "~/.cargo/credentials.toml",
      "access": "read"
    }
  }
}
```

Every reference defines its own path and access in the same policy. Files and
directories are supported. References never inherit another project's policy.
Relative filesystem and reference paths resolve from the active project root;
`~` resolves from the real user home.

Read access to a credential exposes its contents to every process in that
execution zone. Packs never add credential permissions. Because tool HOME is
isolated, CLIs may need an explicit configuration path, such as
`GH_CONFIG_DIR=/Users/me/.config/gh gh ...`; granting a reference does not
change the CLI's search path.

On macOS, Seatbelt enforces denies within allowed directories. Linux Landlock
cannot enforce this combination: Nonaut rejects the policy before launching
and identifies the conflicting deny and grant. Use narrower grants on Linux.

Nonaut protects its application configuration and all loaded policy files from
sandbox writes. A broad writable reference covering Nonaut's configuration
directory is rejected. On Linux a supplied policy inside a writable project is
also rejected; keep that policy outside the granted tree.

## Composition

`extends` imports local files using the same project schema:

```json
{
  "extends": ["./shared/base.json"],
  "packs": ["rust"],
  "filesystem": { "readwrite": ["."] }
}
```

Import paths resolve relative to the declaring file. All permission paths,
including imported ones, resolve from the active project root.

Imports merge left to right, then the declaring file applies. Packs and
filesystem lists combine with duplicates removed. Denies remain active.
Conflicting definitions of a named reference are errors. An explicit network
section replaces the entire inherited network section. The last specified
name, root, and tools section wins; every declared root must match the project.
Cycles, excessive depth, and remote imports are rejected.

## Network

```json
{
  "network": {
    "mode": "allowlist",
    "domains": ["crates.io", "static.crates.io"]
  }
}
```

Modes are `blocked` (the default), `allowlist` (HTTPS through a filtering
proxy), and `open` (unrestricted IP networking). Allowlist mode requires domains;
other modes reject them. A bare `*` is rejected. Cloud metadata and link-local
destinations remain blocked by the proxy.

Provider inference uses the control plane and does not require execution-zone
network permission.

## Inspecting and changing permissions

`/permissions` shows effective project grants, packs, networking and policy
sources. `/permissions full` expands runtime grants and protected configuration
paths. The agent's `sandbox_info` tool exposes the pinned capability manifest.

Edit the project file manually and start a new session to apply changes.
Running sessions retain their resolved policy even if a source file changes.
Policy files are written with mode 0600 in private directories.
