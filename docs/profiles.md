# Runtime packs

Nonaut resolves runtime support internally before creating its Nono worker.
Users do not need Nono profiles or a separate Nono CLI. Integrations declare
read-only installations, executable directories, environment settings and private
state; the resolver validates and pins one plan for enforcement and inspection.

## Selection

New policies use automatic detection:

```json
{
  "runtime": {"mode":"auto", "exclude":["bun"]},
  "filesystem": {"readwrite":["."]}
}
```

Auto mode selects known host integrations and installed toolchains, then adds
language packs suggested by root-level Cargo, Python, Node, Go and Bun markers.
Detection reads file presence and trusted host locations; it does not parse or
execute package scripts. An unavailable tool is reported by `runtime check`.

For explicit selection use `"mode":"manual"` with a `packs` list. For no built-in
runtime support use `"mode":"custom"` with explicit filesystem and environment
settings. See [configuration](configuration.md#runtime-selection-and-overrides).
Without a runtime block, selection defaults to auto. Packs are configured only
under `runtime.packs`.

## Built-in integrations

| Pack | Runtime support |
| --- | --- |
| `system` | OS executables, loaders, libraries, certificates, locales and devices |
| `homebrew` | Discovered Homebrew executables, libraries and OpenSSL config/CA files |
| `apple-sdk` | Selected macOS developer directory and private xcrun cache |
| `gcc` | Linux compiler helpers and public development headers |
| `git` | Git without ambient system configuration |
| `rust` | Cargo/rustup and private Cargo/download caches |
| `python` | Python, pyenv, uv, pip, distro startup files and private caches |
| `node` | Node, NVM, FNM, npm, pnpm, Yarn and Corepack |
| `go` | Go SDK, GOPATH tools, private module/build caches |
| `bun` | Bun installation and private package cache |

Host discovery supports standard locations and trusted absolute `CARGO_HOME`,
`RUSTUP_HOME`, `PYENV_ROOT`, `NVM_DIR`, `FNM_DIR`, `BUN_INSTALL`, `GOROOT`,
`HOMEBREW_PREFIX` and `DEVELOPER_DIR` settings. Homebrew prefixes can also be
recognized from the host PATH. Node prefers active version-manager paths within
recognized installation roots. The selected Apple developer directory otherwise
comes from the fixed system `xcode-select -p` executable.

Automatically discovered installations must be owned by root or the user,
not world-writable, and outside the writable project. Nonaut rejects installation
grants overlapping writable grants. Symlink aliases retain their canonical
bindings; the adapter checks those bindings before granting access. Other layouts
can use explicit runtime grants without broadening automatic discovery.

Explicit packs take PATH priority in listed order. Automatically selected
language installations precede host/package-manager and OS executable paths.
Duplicate or unknown pack names and conflicting pack environment values are
errors. Explicit environment values can override pack settings.

## Isolation

Commands keep a private workspace-scoped HOME:

```text
$XDG_CACHE_HOME/nonaut/homes/<workspace-id>
```

Selected language packs keep writable state under:

```text
$XDG_CACHE_HOME/nonaut/profiles/<pack>/<workspace-id>
```

With no XDG_CACHE_HOME, Nonaut uses `~/.cache`. Private directories are prepared
only after policy validation, and preparation refuses symlink components.
Toolchain installation paths remain read-only. The Rust pack points
`BUNDLED_CLI_CACHE_DIR` to a private cache for downloaded tool archives.
The Apple SDK pack redirects xcrun's database into private temporary state.

Packs never grant credentials, other projects or network access. Configure
network domains separately. Rust downloads usually need `index.crates.io` and
`static.crates.io`. On macOS, native TLS builds may require services intentionally
outside the sandbox; prefer a verified archive cache rather than exposing the
login Keychain or granting arbitrary system services.

## Inspection and checks

```sh
nonaut runtime list
nonaut runtime show --json
nonaut runtime check
nonaut runtime check --config ~/policies/custom.json --json
```

`/runtime`, `/permissions full`, `sandbox_info` and `$NONAUT_CAP_FILE` describe
the same pinned plan: selected mode/packs, selection reasons, grants and their
origins, executable paths, environment names/sources, and diagnostics. Environment
values are hidden. Runtime grants remain distinct from model file-tool access.

Checks execute fixed shell, version, interpreter-startup and SDK probes inside
the selected sandbox.
A failed or unavailable probe makes the command fail; no denial causes automatic
grant expansion. Custom mode has no built-in probes. Check your custom tools with
`nonaut exec --config ... -- '<command>'`. Changes require a new session.
