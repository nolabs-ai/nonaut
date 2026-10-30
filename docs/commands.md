# Commands and keyboard controls

## TUI commands

| Command | Action |
| --- | --- |
| `/provider` | Choose or reconnect a provider |
| `/model` | Select a provider model |
| `/reasoning` | Select reasoning effort |
| `/new` | Start a fresh provider session |
| `/exec [--cwd DIR] <command>` | Run a command directly in Nono |
| `/undo` | Restore files to before the previous turn |
| `/theme` | Choose the interface theme |
| `/clear` | Clear visible conversation output |
| `/details` | Show the most recent tool's complete output |
| `/help` | Show commands |
| `/detach` | Keep the session running and return to the shell |
| `/q` | Quit |

`/exec` does not require a provider connection. It uses the same workspace
policy and sandbox as model-requested commands.

## Keyboard controls

| Key | Action |
| --- | --- |
| `Shift+Enter` | Insert a prompt newline |
| `Up` / `Down` | Move within a prompt or recall prompt history |
| `Esc` | Interrupt an active response |
| `Alt+1` | Open the Agent view |
| `Alt+2` | Open the Review view when changes exist |

Submitting another prompt while a response is streaming interrupts the active
response first.

The mouse wheel scrolls conversation history. Dragging across conversation text
highlights it and copies the selection to the system clipboard, including text
selected while scrolling beyond the viewport. On exit, Nonaut clears its
alternate screen and saved terminal scrollback.

## Agent tools

`sandbox_info` returns the sanitized manifest for the session's pinned Nono
policy. Commands can read the identical JSON through `$NONAUT_CAP_FILE`.
`inspect_code` builds a compact, line-ranged declaration index for Rust,
Python, JavaScript, TypeScript, TSX, and Go. Models are instructed to use it
before broad reads. `batch` runs up to 16 independent read-only file, search,
directory, or index calls concurrently inside the pinned worker; every nested
call is authorized before the batch enters Nono.

Command output is bounded by both bytes and lines. When output is too large,
Nonaut preserves its beginning and end so diagnostics at the bottom are not
lost. Running command cards show elapsed time, their configured deadline, and
the latest output line. Completed cards keep the final diagnostic lines in the
conversation; `/details` reveals the complete bounded output. A timed-out
command also returns output captured before termination.

## Review view

The Review view displays staged, unstaged, and untracked work and refreshes as
the working tree changes. It is hidden when the workspace is clean.

| Key | Action |
| --- | --- |
| Arrow keys | Scroll |
| `PageUp` / `PageDown` | Scroll by page |
| `Tab` / `Shift+Tab` | Move between changed files |
| `/` | Search the diff |
| `Alt+1`, `Esc`, or `q` | Return to the Agent view |

## Headless execution

Run a command through the sandbox without opening the TUI:

```sh
nonaut exec -- "cargo test"
nonaut exec --cwd crates/service -- "cargo test"
```

This is useful for validating workspace policy and reproducing denials.

## Workspaces

An interactive launch from an unconfigured directory offers a one-time session
or a guided saved configuration. Run the same editor explicitly with:

```sh
nonaut setup
nonaut workspace edit nonaut
```

Manage and select named workspaces from any directory:

```sh
nonaut workspace list
nonaut workspace show nonaut
nonaut workspace add ~/Work/nonaut --name nonaut --profile rust
nonaut workspace default nonaut
nonaut --workspace nonaut
nonaut --workspace .
```

The selected workspace remains pinned for the session. `.` means the exact
current directory and bypasses named/default selection.

## Persistent sessions

`/detach` leaves the provider and conversation running in a local worker.
Detached sessions expire after 24 hours without attachment.

```sh
nonaut ps
nonaut attach <session>
nonaut kill <session>
nonaut kill --all
```

Session metadata is stored under `$XDG_STATE_HOME/nonaut/sessions`, defaulting
to `~/.local/state/nonaut/sessions`. Provider credentials are not stored there.
