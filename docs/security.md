# Sandbox and security

Nonaut separates the trusted control plane from model-requested execution.
The TUI, provider authentication, and inference run outside the sandbox. File
tools, searches, edits, and commands run through a Nono worker permanently
confined to the resolved workspace policy.

Nonaut currently embeds Nono 0.80.

## Default boundary

By default, a sandboxed operation receives:

- Read/write access to the active workspace
- A private, writable, per-workspace `HOME`
- Minimal system paths needed to execute ordinary commands
- No real-home credentials or ambient environment variables
- No network access

Development profiles add read-only toolchain paths and isolated writable
caches. Explicit filesystem and network configuration can widen this boundary.

The configuration file remains outside every workspace so an agent cannot
edit its own permissions.

## Sandbox lifetime

At session start, Nonaut resolves the selected workspace, read-only
references, development profiles, explicit filesystem grants, private runtime
paths, and network policy into one capability set. Nono applies that immutable
set once to a long-lived worker; every tool process inherits its Seatbelt or
Landlock boundary.

The configuration is pinned for the session. Changes take effect after a
restart rather than widening a running sandbox. Separate sessions have
separate workers. Interrupting or timing out a call kills the worker process
group; the next call recreates it with the same pinned capability set.

At creation, Nonaut writes a sanitized JSON description of that capability
set outside the writable workspace. Sandboxed commands receive its exact path
as `NONAUT_CAP_FILE`, with read-only access to that file. The model can request
the same data through `sandbox_info`. Both expose model-addressable paths,
runtime-only support paths, network rules, profile names, environment variable
names, the tool timeout, and a policy digest; environment values and secrets
are never included. The manifest describes the boundary, while Nono's
Seatbelt or Landlock policy remains authoritative.

## Filesystem

Nono enforces filesystem capabilities at the operating-system layer. Nonaut
pins one primary workspace when the session starts and passes the resulting
capabilities to Nono. Other configured workspaces remain inaccessible unless
the active workspace names them as read-only references.

The real user home is not used as sandbox `HOME`. Provider credentials, SSH
keys, shell history, cloud configuration, and unrelated projects therefore
remain unavailable unless explicitly granted.

Read or write grants outside the workspace should be narrow. A writable grant
allows the agent to change anything beneath that path; a readable grant can
expose its contents to the provider and, when network is open, to remote
destinations.

A reference prevents writes to its source path. Read access cannot guarantee
that source code is never executed: an agent can ask an interpreter to read it
or copy it into the writable workspace. Treat references as readable context,
not as a separate no-execute boundary.

## Network

Network is blocked by default. Allowlist mode sends HTTPS connections through
a local proxy that validates the requested hostname. Open mode removes the
network boundary and should be reserved for workspaces where unrestricted
egress is acceptable.

Cloud metadata and link-local addresses are denied by the allowlist proxy.

On Linux, allowlist mode requires Landlock network support from Linux 6.7 or
newer. Older kernels keep network fully blocked rather than failing open.
Landlock constrains the proxy port rather than the destination address, so the
proxy port is reachable on other hosts; the proxy still validates requests it
receives.

## Environment and providers

Sandboxed processes start from a cleared environment. Nonaut adds only the
small runtime environment and explicit values produced by development
profiles. Provider credentials remain in the control plane and are never
copied into detached-session metadata.

Provider inference itself is not sandboxed: prompts, approved project files,
and tool results are sent to the selected provider as required for the agent
to operate.

## Project instructions

Nonaut can read `AGENTS.md` or `CLAUDE.md` before sandbox execution. It accepts
only a regular file with one hard link and limits it to 32 KiB. Symlinks and
hard links are ignored to prevent a writable workspace from redirecting the
control plane to sensitive host files.

## Undo

Nonaut starts a snapshot in the background with each agent turn and `/exec`,
overlapping the filesystem walk with model inference. Before the first command
or file mutation can execute, it waits for that snapshot if necessary;
concurrent mutations share the same barrier. `/undo` restores the latest turn
and can be repeated to step back further.

`.git`, `target`, `node_modules`, and files ignored by `.gitignore` are not
tracked. Snapshots live under `$XDG_STATE_HOME/nonaut/undo`, outside sandbox
reach, and are removed when Nonaut exits. When `XDG_STATE_HOME` is unset,
Nonaut uses `~/.local/state`.

Undo is a convenience recovery mechanism, not a substitute for version
control or backups.
