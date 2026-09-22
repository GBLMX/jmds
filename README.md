# jmds

**Just My DeepSeek** — a terminal coding harness for one model, in Rust.

Four tools, one conversation, and a real shell. Built for reading DeepSeek's reasoning and tool
calls in a terminal next to an editor and a shell, rather than for being everything to everyone: no
plugin system, no language server, no subagent fleet, no marketplace. What is here is small enough
to read in an afternoon.

## What it does

- **One chat pane** that streams answers and `reasoning_content` apart, shows tool calls as they
  happen, folds long reasoning instead of hiding it, and stays scrolled where you put it.
- **Four tools**: `read`, `write`, `edit`, `bash`. The file tools match exact text against the file
  as it is on disk; `bash` runs one non-interactive command with a timeout it will not silently
  shorten, and keeps long output in a file it names rather than dropping it.
- **Sessions as append-only JSONL** under `~/.config/jmds/sessions/`, written a line at a time, with
  recovery that stops at the first line it cannot read instead of papering over a hole.
- **Themes** that downsample themselves to whatever the terminal can actually display, with an
  ASCII glyph set for terminals where box drawing comes out wrong.

## Build and run

```sh
cargo build --release
DEEPSEEK_API_KEY=sk-… ./target/release/jmds
```

The key is read from the environment variable named in the config file, never from the file itself:
`~/.config/jmds/config.toml`, written on first save, and read on every start. A missing key is not
a crash — the app runs, and says so in the transcript when you ask something.

### Keys

| Key | What it does |
|---|---|
| `Enter` | send the input line |
| `Up` / `Down` | walk the history of what you have sent |
| `PageUp` / `PageDown` | scroll the transcript |
| `Ctrl+End` / `Ctrl+Home` | follow the newest output again / jump to the top |
| `Ctrl+Q` | quit |
| `Ctrl+W` | close the focused pane |
| `Ctrl+T` | move focus to the next pane |
| `Alt+1`…`Alt+9` | jump to a pane by position |

Keys are offered to the focused pane first, except the two that quit and close: a pane can refuse a
key, but it cannot trap you.

## Layout

| Crate | What lives there |
|---|---|
| `crates/jmds-api` | The DeepSeek client: the request shape, SSE framing, usage normalisation, and the retry rule that stops once the answer has started. |
| `crates/jmds-core` | The engine: the event bus, the pane tree, configuration, sessions, the turn loop, and the four tools. No terminal library. |
| `crates/jmds-tui` | The terminal: capability probes, the theme, the effects, the `Pane` trait and the host that lays panes out. No model. |
| `crates/jmds` | The binary: the event loop, the clock, the terminal's modes, and the one place the three meet. |

## Credits

jmds is MIT. Parts of it are ported from another project, and several decisions were learned from
two more; [`CREDITS.md`](CREDITS.md) says exactly which, file by file, and
[`THIRD_PARTY_LICENSES/`](THIRD_PARTY_LICENSES/) carries the notices those licences require.
