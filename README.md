# Super Tabs for Zellij

Schema-driven vertical tabs for Zellij with aligned multi-column metadata, pane-targeted CLI updates, and tab-name-backed session recovery.

## Features

- Static column schema with widths aligned across all tabs in the session, including exact fixed-width columns
- Multi-color cell content using tmux-style `#[...]` inline styles
- `super-tabs set ...` CLI updates routed by pane id
- Incremental per-column width tracking instead of full rescans on each update
- Plain-text persistence by mirroring column values into the real Zellij tab name
- Existing vertical-tab behaviors kept in place: border, overflow, mouse selection, scrolling, and fullscreen/sync/active indicators

## Requirements

- Zellij 0.40.0 or later
- Rust with the `wasm32-wasip1` target, or the included `direnv` + `shell.nix` setup

## Local Tooling

This repo includes `.envrc` and `shell.nix`.

```bash
direnv allow
```

That shell provides Rust, `rustfmt`, `clippy`, `rust-analyzer`, and the `wasm32-wasip1` target.

## Permissions

The plugin requests:

- `ReadApplicationState`
- `ChangeApplicationState`
- `FullHdAccess`

Grant them the first time Zellij prompts.

`FullHdAccess` is used to mount a shared host state folder at `/host`, so multiple Super Tabs plugin instances can read the same persisted raw cell state. By default the host folder is `$XDG_DATA_HOME` or `$HOME/.local/share`; the plugin writes under the `super-tabs/` subdirectory. The host folder itself must already exist. You can override it with `state_host_folder`.

## Build

The workspace contains:

- `.`: the Zellij plugin crate
- `core/`: shared parsing, layout, protocol, and persistence logic
- `cli/`: the native `super-tabs` command

The repo default target is `wasm32-wasip1`, so the plugin build is straightforward:

```bash
cargo build --release -p super-tabs --target wasm32-wasip1
```

Build the CLI for your host target separately:

```bash
cargo build --release -p super-tabs-cli --target <host-triple>
```

Examples:

- Apple Silicon macOS: `aarch64-apple-darwin`
- Intel Linux: `x86_64-unknown-linux-gnu`

## Install

Copy the plugin wasm somewhere Zellij can load it:

```bash
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/super-tabs.wasm ~/.config/zellij/plugins/
```

If you want the CLI on your `PATH`, copy or symlink the built binary from your host target directory.

## Quick Start

Use one of the included layouts:

```bash
zellij --layout examples/super-tabs-left.kdl
zellij --layout examples/super-tabs-right.kdl
zellij --layout examples/tmux-style.kdl
zellij --layout examples/tmux-colored.kdl
```

Note: Zellij's documented file-plugin form is `file:/absolute/path/to/plugin.wasm`. Do not use `~` in the plugin URL.

Then update tab-owned column state from inside any Zellij pane:

```bash
super-tabs set branch="main"
super-tabs set status='#[fg=red,bold]dirty'
super-tabs set title='api | worker'
super-tabs set --pane 12 branch="release"
super-tabs set status=""
```

## Layout Configuration

```kdl
layout {
    pane split_direction="vertical" {
        pane size=30 borderless=true {
            plugin location="file:/absolute/path/to/super-tabs.wasm" {
                columns "branch,status,title"

                column_branch "resize=trunc:end:hard:12;style=#[fg=blue]"
                column_status "resize=trunc:end:fixed:10;style=#[fg=yellow]"
                column_title  "resize=resize;style=#[fg=muted]"

                border "#[fg=dim]│"
                padding_top 0
                overflow_above "  ^ +{count}"
                overflow_below "  v +{count}"
                state_host_folder "/Users/you/.local/share"
            }
        }
        pane
    }
}
```

### Column Definition Fields

- `resize=resize`
- `resize=trunc:start:flow:N`
- `resize=trunc:end:flow:N`
- `resize=trunc:start:hard:N`
- `resize=trunc:end:hard:N`
- `resize=trunc:start:fixed:N`
- `resize=trunc:end:fixed:N`
- `style=#[...]`

`hard` caps a column at `N` cells but still lets it shrink when every value is shorter. `fixed` always reserves exactly `N` cells for that column and truncates using the configured side when content is longer.

### Indicator Configuration

These existing config keys still work:

- `indicator_active`
- `indicator_fullscreen`
- `indicator_sync`
- `border`
- `padding_top`
- `overflow_above`
- `overflow_below`

## Persistence Model

Super Tabs mirrors plain-text column values into the real Zellij tab name using a keyed quoted format like this:

```text
branch="main" | status="dirty" | title="api | worker"
```

That lets the plugin hydrate plain-text state after restart or resurrection even if shared filesystem state is unavailable.

When the shared host state folder is mounted, Super Tabs also persists raw cell values there, including inline style overrides. Writes use a temp file in the same directory followed by an atomic rename, so readers either see the previous complete JSON document or the next complete JSON document.

## Edge Cases

- Tabs with manual names that do not match the keyed Super Tabs format are treated as unmanaged until the first Super Tabs write.
- External manual renames after a tab is managed are not adopted back into the schema state.
- If the pane is too narrow, the plugin hard-clips the rendered row at the right edge rather than wrapping.

## Development Checks

```bash
cargo check
cargo test -p super-tabs-core --target <host-triple>
cargo test -p super-tabs-cli --target <host-triple>
```
