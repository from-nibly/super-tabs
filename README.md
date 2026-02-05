# Vertical Tab Bar for Zellij

A zellij plugin that displays tabs vertically as rows on the left or right side of the screen, instead of the default horizontal tab bar at the top.

## Why Vertical Tabs?

Horizontal tab bars become hard to read when you have many tabs - names get truncated and it's difficult to see all your tabs at a glance. A vertical tab bar shows each tab on its own row, making it easy to see all tab names and quickly navigate between them.

```
┌──────────────┬──────────────────────────────┐
│ 1:shell*     │                              │
│ 2:server     │  Your terminal content here  │
│ 3:logs       │                              │
│ 4:editor     │                              │
│ 5:htop       │                              │
└──────────────┴──────────────────────────────┘
```

## Features

- **Active tab highlighting** - Currently selected tab is visually distinct
- **Mouse support** - Click any tab to switch to it
- **Scroll wheel** - Scroll up/down over the tab bar to navigate tabs
- **Overflow indicators** - `^ +N` and `v +N` for hidden tabs above/below
- **Smart viewport** - Active tab always stays visible, viewport scrolls automatically
- **Status indicators**:
  - `*` - Active tab
  - `Z` - Fullscreen mode active
  - `S` - Sync panes mode active
- **Tab numbering** - Each tab prefixed with its number (e.g., `1:shell`)
- **Name truncation** - Long tab names truncated with `...` to fit width
- **Tmux-style formatting** - Inline color syntax like `#[fg=accent]`
- **Pane title support** - Display focused pane's terminal title via `{title}`

## Requirements

- [Zellij](https://zellij.dev/) v0.40.0 or later
- [Rust](https://rustup.rs/) toolchain (for building from source)

## Permissions

This plugin requires the following permissions:

- **ReadApplicationState** - Required to receive tab information
- **ChangeApplicationState** - Required to switch tabs when you click them

On first run, zellij will prompt you to grant permissions. Focus the plugin pane and press `y` to grant.

## Installation

### Building from Source

1. **Install the WebAssembly target** (one-time setup):
   ```bash
   rustup target add wasm32-wasip1
   ```

2. **Clone and build**:
   ```bash
   git clone <repository-url>
   cd zellij-vertical-tabs
   cargo build --release
   ```

3. **Locate the built plugin**:
   ```
   target/wasm32-wasip1/release/zellij-vertical-tabs.wasm
   ```

### Installing the Plugin

Copy the `.wasm` file to your zellij plugin directory:

```bash
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/zellij-vertical-tabs.wasm ~/.config/zellij/plugins/
```

## Usage

### Quick Start

Use one of the included layout files:

```bash
# Tab bar on the LEFT side
zellij --layout layouts/vertical-tabs-left.kdl

# Tab bar on the RIGHT side
zellij --layout layouts/vertical-tabs-right.kdl

# Tmux-style (minimal, with pane titles)
zellij --layout layouts/tmux-style.kdl

# Tmux-style with colors (demonstrates inline color syntax)
zellij --layout layouts/tmux-colored.kdl
```

### Custom Layouts

Create your own layout file to customize the tab bar width and position.

#### Tab Bar on Left (18 columns wide)

```kdl
// ~/.config/zellij/layouts/my-layout.kdl
layout {
    pane split_direction="vertical" {
        pane size=18 borderless=true {
            plugin location="file:~/.config/zellij/plugins/zellij-vertical-tabs.wasm"
        }
        pane
    }
    pane size=1 borderless=true {
        plugin location="zellij:status-bar"
    }
}
```

#### Tab Bar on Right (20 columns wide)

```kdl
layout {
    pane split_direction="vertical" {
        pane  // Main content
        pane size=20 borderless=true {
            plugin location="file:~/.config/zellij/plugins/zellij-vertical-tabs.wasm"
        }
    }
    pane size=1 borderless=true {
        plugin location="zellij:status-bar"
    }
}
```

#### Without Status Bar (maximized space)

```kdl
layout {
    pane split_direction="vertical" {
        pane size=15 borderless=true {
            plugin location="file:~/.config/zellij/plugins/zellij-vertical-tabs.wasm"
        }
        pane
    }
}
```

### Setting as Default Layout

To always use vertical tabs, set the layout in your zellij config:

```kdl
// ~/.config/zellij/config.kdl
default_layout "vertical-tabs-left"
```

Then save your layout file as `~/.config/zellij/layouts/vertical-tabs-left.kdl`.

---

## Configuration Options

Configure the plugin in your layout file:

```kdl
plugin location="file:~/.config/zellij/plugins/zellij-vertical-tabs.wasm" {
    // Tab format (inactive tabs)
    format "{index}:{name}"

    // Active tab format
    format_active "{index}:{name}*"

    // Status indicators
    indicator_active "*"
    indicator_fullscreen "Z"
    indicator_sync "S"

    // Maximum name length before truncation
    max_name_length 15

    // Right border (with optional color)
    border "#[fg=dim]│"

    // Start tab numbering from (default: 1)
    start_index 1

    // Empty rows above the tab list (default: 0)
    padding_top 0

    // Overflow indicator formats (when tabs don't fit)
    overflow_above "  ^ +{count}"
    overflow_below "  v +{count}"
}
```

### Format Variables

| Variable | Aliases | Description |
|----------|---------|-------------|
| `{index}` | `{i}` | Tab number |
| `{name}` | `{n}` | Tab name |
| `{title}` | `{t}` | Focused pane's terminal title |
| `{indicators}` | | Combined status indicators |
| `{fullscreen}` | | Fullscreen indicator if active |
| `{sync}` | | Sync indicator if active |
| `{active}` | | Active indicator if current tab |

### Inline Color Syntax

Use tmux-style inline colors in format strings:

```
#[fg=color]           - Set foreground color
#[bg=color]           - Set background color
#[fg=color,bg=color]  - Set both colors
#[fg=color,dim]       - Set color with dim attribute
#[fg=color,bold]      - Set color with bold attribute
#[bg=color,fill]      - Fill entire row with background color
#[fg=none]            - Reset to default
```

**Active tab row highlighting:**

The `format_active` style controls how the active tab row looks:

```kdl
// Fill entire row with dark gray background
format_active "#[bg=236,fill]{index}:{title}*"

// Background color on text only (not padded to edge)
format_active "#[bg=236]{index}:{title}*"

// No row highlight, just colored text
format_active "#[fg=green]{index}:{title}*"
```

**Color formats:**

| Format | Example | Description |
|--------|---------|-------------|
| Named | `fg=dim` | Predefined color names |
| 256-color | `fg=238` | 256-color palette (0-255) |
| Hex RGB | `fg=#444444` | RGB hex (6 digits) |
| Short hex | `fg=#444` | RGB hex (3 digits, expanded) |
| RGB function | `fg=rgb(68,68,68)` | RGB values |

**Named colors:**

| Name | Aliases | Description |
|------|---------|-------------|
| `accent` | `primary` | Bright blue (39) |
| `secondary` | | Light blue (75) |
| `tertiary` | | Purple (141) |
| `muted` | `quaternary` | Light gray (245) |
| `dim` | `dimmed` | Dark gray (240) |
| `red` | `error`, `warning` | Red (196) |
| `green` | `success`, `ok` | Green (82) |
| `black`, `white`, `yellow`, `blue`, `magenta`, `cyan`, `orange`, `gray`, `pink`, `purple` | | Standard colors |
| `none` | `default`, `reset` | Terminal default |

### Truncation

Use `{=width:var}` to truncate a variable to a specific width:

```kdl
format "{index}:{=12:title}"  // Truncate title to 12 chars
```

---

## Mouse and Keyboard Interaction

| Action | Effect |
|--------|--------|
| **Left click** on tab | Switch to that tab |
| **Left click** on `^ +N` | Scroll view up / switch to tab above |
| **Left click** on `v +N` | Scroll view down / switch to tab below |
| **Scroll wheel up** | Switch to previous tab |
| **Scroll wheel down** | Switch to next tab |

Note: Standard zellij keybindings for tab management still work (e.g., `Ctrl+t` then `n` for new tab).

## Tab Display Format

Each tab is displayed as:

```
N:name INDICATORS
```

Where:
- `N` - Tab number (1-indexed by default)
- `name` - Tab name (truncated with `...` if too long)
- `INDICATORS` - Status flags:
  - `*` appears when tab is active
  - `Z` appears when fullscreen mode is active in that tab
  - `S` appears when sync panes mode is active

### Examples

```
1:shell*        <- Active tab
2:server        <- Inactive tab
3:my-very-lo... <- Truncated name
4:build Z       <- Fullscreen active
5:terminals S   <- Sync panes active
```

## Overflow Behavior

When you have more tabs than can fit in the available rows, the plugin shows overflow indicators:

```
  ^ +3           <- 3 tabs hidden above (click to scroll up)
4:current*
5:server
6:logs
  v +2           <- 2 tabs hidden below (click to scroll down)
```

The viewport automatically scrolls to keep the active tab visible when you switch tabs.

---

## Troubleshooting

### Plugin doesn't load

- Check the path in your layout file is correct
- Ensure the `.wasm` file exists at that path
- Try using an absolute path instead of `~`

### Tab bar is empty / No tabs showing

- **Most likely cause**: You haven't granted permissions yet
- When zellij prompts for permissions, press `y` to grant
- The plugin requires `ReadApplicationState` permission to receive tab info
- Check if you see a permission prompt in the status bar

### Tabs not updating

- Verify the plugin is receiving events
- Ensure `request_permission` is called before `subscribe`

### Click not working

- Ensure `set_selectable(false)` is called in the plugin
- Verify mouse events are being received

### New tabs show wrong title

This is a known limitation. When a new tab is created, zellij sends the PaneUpdate event before the shell has set the terminal title via ANSI escape sequences. The plugin will show "..." until the title is available (usually after switching tabs or creating another tab).

## Resources

- [Zellij Documentation](https://zellij.dev/documentation/)
- [Zellij Plugin Development](https://zellij.dev/documentation/plugins)
- [zellij-tile crate docs](https://docs.rs/zellij-tile)
- [Zellij GitHub](https://github.com/zellij-org/zellij)

## License

MIT
