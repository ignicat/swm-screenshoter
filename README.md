# swm-screenshoter

[![Release](https://img.shields.io/github/v/release/ignicat/swm-screenshoter?style=flat-square)](https://github.com/ignicat/swm-screenshoter/releases/latest)
![Rust](https://img.shields.io/badge/rust-1.97.1%2B-orange?style=flat-square)
![Wayland](https://img.shields.io/badge/protocol-Wayland-blue?style=flat-square)

Screenshot tool for [swm](https://github.com/ignicat/swm) (simple window manager): a CLI, a freeze-frame picker overlay and an `org.freedesktop.impl.portal.Screenshot` backend in one binary.

It lives outside `swm` because we tried to stick to the Unix way: PNG encoding, pickers, toasts and the screenshot portal have no business inside a window manager. That said, a usable Wayland screenshot tool still does quite a lot on its own: a freeze-frame `zwlr_layer_shell_v1` overlay, region/screen/window pickers, a preview toast, a detached clipboard helper and a `swm.rune` reader. So it's a practical split rather than a strictly single-purpose tool, but it keeps `swm` itself lean.

## How it works

- **Capture over swm IPC.** Talks to `$XDG_RUNTIME_DIR/swm/swm.sock` (swm IPC v2): `CaptureFrame` for monitors and windows (pixels arrive in a `memfd` via `SCM_RIGHTS`), plus `Outputs`, `Node` and `ConfigPath`. swm still enforces the session lock and `block-screencast` window/layer rules on every frame.
- **Freeze-frame.** All outputs are grabbed at the moment you start, so menus, tooltips and animations stay frozen while you pick. Uneven multi-monitor layouts (empty space around them is trimmed) and fractional HiDPI scales (`wp_viewporter`) are handled.
- **Window picking.** Finds the window under the pointer from `NodeInfo.screen` rects and stacking order, then captures it cleanly (with decorations) through `CaptureSource::Window`.
- **Remembers your region.** The last region is stored in `$XDG_STATE_HOME/swm/screenshoter-region.state` (or `~/.local/state/swm/screenshoter-region.state`).
- **Output files.** `swm-screenshot-<secs>-<millis>-<n>.png` in `$XDG_PICTURES_DIR/Screenshots` (or `~/Pictures/Screenshots`), or an exact path via `-p`/`--path`.
- **Clipboard.** Serves `image/png` over `ext-data-control-v1` from a small forked `--serve-clipboard` process until something else takes the clipboard. Falls back to `wl-copy` when swm runs without `SWM_ALLOW_CLIPBOARD_CONTROL=1`.

## Build & install

No `swm` checkout needed:

```sh
git clone https://github.com/ignicat/swm-screenshoter.git
cd swm-screenshoter
cargo build --release --locked
install -Dm755 target/release/swm-screenshoter ~/.local/bin/swm-screenshoter
```

Bind it in `swm.rune`:

```rune
keybinds:
  "Print" "swm-screenshoter"
  "Shift+Print" "swm-screenshoter desktop --clipboard"
end
```

## CLI

Without arguments it opens the `menu`.

```sh
swm-screenshoter [COMMAND] [OPTIONS]
swm-screenshoter --portal
swm-screenshoter --serve-clipboard
```

| Command | Action |
| --- | --- |
| `menu` | Pick `Region` / `Screen` / `Window` interactively (default) |
| `region [--rect X Y W H]` | Drag/resize a region, or capture an exact logical rect without UI |
| `screen` | Capture one monitor (asks which one if there are several, unless `-o` is given) |
| `desktop` | Stitch all outputs into one PNG (ignores `-o`) |
| `window [NODE_ID]` | Pick a window, or capture `NODE_ID` without UI |

Options for the capture commands:

| Option | Effect |
| --- | --- |
| `-o, --output <NAME>` | Limit the capture or picker to one output |
| `--rect <X Y W H>` | Exact logical geometry (`region` only) |
| `--cursor` | Include the cursor |
| `--clipboard` | Copy the PNG to the clipboard |
| `--silent` | Don't show the preview toast |
| `-p, --path <FILE>` | Save to this file instead of the timestamped name |
| `-h, --help`, `-V, --version` | Usage / version |

`--serve-clipboard` is an internal helper started by `--clipboard` or the toast's `Copy` button so the clipboard survives after the CLI exits. `--portal` runs the D-Bus Screenshot portal daemon.

## Overlay controls

- **Menu**: `Left`/`Up`/`Shift+Tab` and `Right`/`Down`/`Tab` (or hover/click) switch between `Region`, `Screen` and `Window`. `Enter`/`Space` confirms, `Esc` cancels.
- **Region**: drag the area or its edges/corners. Arrow keys move it by `10px` (`Ctrl+Arrow` by `1px`), `Shift+Arrow` resizes by the same step. `Enter`/`Space` captures, `Esc` goes back to the menu (if you came from it) or cancels.
- **Screen / Window**: `Left`/`Up`/`Shift+Tab` and `Right`/`Down`/`Tab` cycle through candidates. Click or `Enter`/`Space` captures, `Esc` goes back or cancels.
- **Preview toast**: thumbnail with `Copy` and `Open`. Disappears after `overlays.notifications.success-duration-ms` (default `4000`).

## Theme & config

On start it asks swm for the active config path (`ConfigPath` IPC, fallback `~/.config/swm/swm.rune`) and then applies `~/.config/swm/swm-screenshoter.rune` on top, if present (`$XDG_CONFIG_HOME` is respected).

Keys it reads (`overlay.*` and `*-color` spellings work too):

- `screenshot.directory`: where PNGs go (supports `~/`, `$HOME/`, `$env.HOME/`)
- `font.family`, `font.size`
- `overlays.{background-colour, text-colour, error-colour, border-colour, borders, border-size, radius}`
- `overlays.notifications.{position, offset-x, offset-y, success-duration-ms}`
- When a colour mode is `"system"`, it reads `color-scheme` from `org.freedesktop.portal.Settings`.

Config files use rune block syntax, not dotted keys:

```rune
screenshot:
  directory "~/Pictures/Screenshots"
end
```

## Screenshot portal

With `--portal`, `interactive=true` requests open the menu and toast, while `interactive=false` captures the whole desktop silently and returns a `file://` URI. `cursor=true` works in both. Closing the portal `Request` cancels a capture in progress (after checking the D-Bus sender).

Unit files are in [`packaging/`](packaging/):

| File | Install to |
| --- | --- |
| `packaging/xdg-desktop-portal/portals/swm-screenshoter.portal` | `/usr/share/xdg-desktop-portal/portals/` |
| `packaging/dbus-1/services/org.freedesktop.impl.portal.desktop.swm-screenshoter.service` | `/usr/share/dbus-1/services/` |
| `packaging/systemd-user/swm-screenshoter-portal.service` | `/usr/lib/systemd/user/` |

`swm` ships `swm-portals.conf` with `Screenshot=swm-screenshoter;gtk;`, so both packages install side by side without file conflicts, and `xdg-desktop-portal` falls back to `gtk` when `swm-screenshoter` isn't installed. On older setups without that line, copy [`swm-portals.conf.example`](packaging/xdg-desktop-portal/swm-portals.conf.example) to `~/.config/xdg-desktop-portal/swm-portals.conf`.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps
cargo test --locked --all-targets
```

## License

[GPL-3.0-only](LICENSE)
