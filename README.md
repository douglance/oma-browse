# oma-browse

An Omarchy-themed, agent-drivable browser for Linux. Rust, Tauri and WebKitGTK.

Two ideas, and everything else follows from them:

**There is no toolbar.** The page owns the window. The URL bar, the tab list,
history, bookmarks and every setting live in a command palette that is summoned
over the page and dismissed again, the way a TUI does it. The only permanent
chrome is a 26px strip of favicons at the top, and it floats — the page scrolls
underneath it.

**Every capability is one command, reachable three ways.** The palette, the
keyboard, and a loopback HTTP control plane all dispatch through the same
command graph. Nothing the UI can do is unavailable to a script or an agent, and
nothing has to be implemented twice to keep it that way.

## Theming

The browser dresses itself in the current [Omarchy](https://omarchy.org) theme,
read from `~/.local/state/omarchy/current/theme` and re-read when it changes —
no restart, no D-Bus, no hook to install. Its own chrome takes the theme's
tokens directly. Loaded websites get a stylesheet injected on top of theirs:
links, form controls, the caret, the focus ring, selection and scrollbars
always, and — on by default, `theme recolor off` to stop it — neutral surfaces
repainted onto the theme's ramp, leaving anything with brand colour in it alone.

Window translucency follows Ghostty's `background-opacity`, so the browser is as
see-through as your terminal is.

## Building

Needs GTK 3, WebKitGTK 4.1 and a Rust toolchain (see `rust-toolchain.toml`).
The chrome is [Topcoat](https://crates.io/crates/topcoat), whose client runtime
is bundled out of the compiled binary rather than shipped as a file, so the
bundle step is not optional — without it the palette renders blank:

```sh
cargo build --release
cargo install topcoat-cli          # once
topcoat asset bundle -p oma-browse
```

Two things worth having: a **Nerd Font** (the strip's globe and gear are
`nf-fa-globe` and `nf-fa-cog`), and **gst-plugins-good** — without it WebKit has
no audio sink and media-heavy sites come up blank rather than merely silent.

## Running

```sh
oma-browse                          # the start page
oma-browse https://example.com      # straight to a URL
oma-browse --incognito              # forgets where it has been
```

`--private` is accepted as well, so `omarchy-launch-browser` can hand this
binary a URL and have it do the right thing.

### Keys

| | |
|---|---|
| `Ctrl-K` / `Ctrl-L` / `Ctrl-P` | palette — the URL bar, the tab list, everything |
| `Ctrl-T` / `Ctrl-W` | new tab / close tab |
| `Ctrl-Shift-T` | reopen the last closed tab |
| `Ctrl-Tab` / `Ctrl-Shift-Tab` | next / previous tab |
| `Ctrl-F`, then `Ctrl-G` / `Ctrl-Shift-G` | find, next, previous |
| `Ctrl-D` | bookmark this page |
| `Alt-←` / `Alt-→` / `Alt-Home` | back, forward, home |
| `Ctrl-R`, `F5` | reload |
| `Ctrl-+` / `Ctrl--` / `Ctrl-0` | zoom |
| `F11` | fullscreen |
| `Esc` | dismiss the palette, or stop loading |
| `Ctrl-Shift-W` | close the window |

Keys are bound on the GTK toplevel rather than injected into the page, so they
work on a site that blocks scripts and on whatever currently holds focus.

## Driving it

Every command is also an HTTP route on a loopback control plane, which is what
a script or an agent should talk to. The port is ephemeral and logged at
startup (`control plane up addr=127.0.0.1:…`):

```sh
curl "http://127.0.0.1:$PORT/cmd/tab/open?url=example.com"
curl "http://127.0.0.1:$PORT/cmd/tab/list"
curl "http://127.0.0.1:$PORT/cmd/page/screenshot?path=/tmp/shot.png"
curl --get "http://127.0.0.1:$PORT/cmd/page/eval" --data-urlencode 'js=document.title'
```

The plane is bound to loopback and nothing else: it can read page content and
drive the browser, so it must never be reachable off-host.

Screenshots are taken by WebKit itself rather than by a compositor grabber, so
they work while the window is on another workspace, need no geometry lookup, and
cannot accidentally capture something else on screen.

`oma-browse <command>` runs the same graph in-process — useful for the commands
that need no window (`theme show`, `theme css`), but a second process has no
browser in it, so anything touching a tab answers *the window is not up yet*.
Reach a running browser over HTTP.

Run `oma-browse --llms` for the machine-readable manifest of the whole command
graph, or `oma-browse mcp add` to register it with an MCP client.

## Layout

- `crates/oma-browse` — the browser: window and GTK surgery, the tab model, the
  palette and strip, the command graph, the control plane.
- `crates/oma-theme` — reads an Omarchy theme and renders it as CSS: the token
  block for our own chrome, and the runtime injected into loaded pages.

## Licence

MIT. See [LICENSE](LICENSE).
