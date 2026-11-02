# Commands and keyboard controls

The project panel shows the project directory, the policy source file and
connection status. Use `/permissions` for the full resolved boundary;
capabilities are not summarized in the panel.
An amber dot indicates connection or sandbox preparation. A green dot appears
only after both are ready, with no success text. When disconnected, a red dot
points to `/provider` to select a provider or sign in. First launch opens the
provider picker automatically when no provider is saved. Failures stay explicit;
routine startup status does not add messages to the conversation.

## TUI commands

| Command | Action |
| --- | --- |
| `/provider` | Choose or reconnect a provider |
| `/model` | Select a provider model |
| `/reasoning` | Select reasoning effort |
| `/new` | Start a fresh provider session |
| `/exec [--cwd DIR] <command>` | Run a command directly in Nono |
| `/permissions [full]` | Show effective access; expand runtime grants with `full` |
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
| `Cmd+C` / `Ctrl+Shift+C` | Copy the selection, prompt, or visible screen |
| `Cmd+V` / `Ctrl+Shift+V` | Paste text without submitting |
| `Alt+M` | Toggle native terminal selection |

Submitting another prompt while a response is streaming interrupts the active
response first.

Drag across any visible text to select and automatically copy it: project and
policy paths, conversation, prompt, dialogs, or Review. Conversation selection
also includes text selected while scrolling beyond the viewport. Keyboard copy
uses the active selection; without one it copies the prompt, or the visible
screen if the prompt is empty. `Ctrl+C` copies an active selection and otherwise
retains its interrupt/quit behavior. `Esc` clears a selection when idle.

Some terminals consume `Cmd+C` or `Ctrl+Shift+C` before Nonaut receives them.
Use `Alt+M` to release mouse capture, then drag and use the terminal's native
copy shortcut. Press `Alt+M` again to restore app scrolling and selection.
On macOS, Nonaut uses the native clipboard. On Linux it tries `wl-copy` /
`wl-paste` or `xclip`; copying falls back to OSC 52, including over SSH.
Terminal paste works through bracketed paste even without a clipboard helper.

On exit, Nonaut clears its alternate screen and saved terminal scrollback.

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

## Project policies

The first interactive launch proposes and saves a minimal project policy after
confirmation. Print the resolved policy and its source path with:

```sh
nonaut config
```

Normal launches discover the project root from the current directory.
Use a supplied policy instead of the saved project policy with:

```sh
nonaut --config ~/policies/rust.json
nonaut exec --config ~/policies/rust.json -- "cargo test"
```

The selected policy remains pinned for the session. An optional `root` must
match the canonical discovered project root. See [configuration](configuration.md).

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
