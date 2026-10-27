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
nonaut exec --cwd ../another-project -- "make check"
```

This is useful for validating workspace policy and reproducing denials.

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
