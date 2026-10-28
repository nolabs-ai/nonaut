# Nonaut documentation

Nonaut is a terminal agent harness that routes model-requested tools through
the Nono operating-system sandbox. These guides document the user-facing
behavior of the current MVP.

## Use Nonaut

- [Getting started](getting-started.md) — install, authenticate, and open a project
- [Commands](commands.md) — TUI commands, review controls, headless execution, and sessions
- [Providers](providers.md) — supported services, authentication, and custom endpoints

## Configure the sandbox

- [Configuration](configuration.md) — XDG config, global defaults, and workspace rules
- [Development profiles](profiles.md) — toolchains, private caches, and profile composition
- [Sandbox and security](security.md) — trust boundaries, filesystem, network, and undo

## Contribute

- [Development and releases](development.md) — source builds, checks,
  installation channels, and releases

The configuration file is intentionally stored outside project workspaces so
an agent cannot edit its own policy.
