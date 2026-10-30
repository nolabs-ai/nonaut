# Development profiles

Profiles expose installed development toolchains without exposing the real
user home. They add only the required read-only toolchain paths, executable
directories, environment variables, and private writable caches.

## Built-in profiles

| Profile | Toolchains and package managers |
| --- | --- |
| `rust` | Cargo and rustup |
| `python` | Python, pyenv, uv, and pip |
| `node` | Node.js, NVM, FNM, npm, pnpm, Yarn, and Corepack |
| `go` | Go SDK, GOPATH tools, modules, and build cache |
| `bun` | Bun runtime and package cache |

## Configure profiles

Profiles are selected per workspace:

```json
"workspace": {
  "definitions": {
    "rust-service": {
      "path": "~/Work/rust-service",
      "profiles": ["rust"]
    },
    "web-app": {
      "path": "~/Work/web-app",
      "profiles": ["node"]
    }
  }
}
```

Polyglot projects can compose profiles:

```json
"product": {
  "path": "~/Work/product",
  "profiles": ["python", "node", "go"]
}
```

Profile `PATH` entries follow the listed order. Duplicate names are rejected.
If future profiles define incompatible values for the same environment
variable, configuration fails instead of silently choosing one.

## Isolation behavior

Toolchain installations such as `~/.cargo/bin`, `~/.rustup`, NVM versions,
or `~/.bun/bin` are read-only. Writable package and build caches live under:

```text
$XDG_CACHE_HOME/nonaut/profiles/<profile>/<workspace-id>
```

When `XDG_CACHE_HOME` is unset, Nonaut uses `~/.cache`.

The Rust profile also points `BUNDLED_CLI_CACHE_DIR` into this cache. Build
scripts that honor it can reuse verified downloaded tool archives across
commands instead of downloading them for every private `HOME`.

Commands also receive a private workspace-scoped `HOME` under:

```text
$XDG_CACHE_HOME/nonaut/homes/<workspace-id>
```

This allows workflows such as `cargo init`, Git configuration, builds, and
package downloads without granting access to the real home or allowing one
project to poison another project's cache.

Profiles never enable network access. Configure the required domains in the
workspace's [network policy](configuration.md#network). For example, Rust
dependency downloads normally require:

```json
"network": {
  "mode": "allowlist",
  "allow_domains": ["index.crates.io", "static.crates.io"]
}
```

On macOS, a build script that insists on Security.framework/native-tls may
still fail its first HTTPS download because Nonaut deliberately does not expose
the login Keychain or security service to arbitrary commands. Do not weaken the
sandbox to fix that. Populate a build script's verified archive cache from a
trusted host build, then sandboxed builds reuse it. For the Rust profile, print
the exact workspace cache destination with:

```sh
nonaut --workspace NAME exec -- 'printf %s "$BUNDLED_CLI_CACHE_DIR"'
```
