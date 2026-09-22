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
  recovery that stops at the first line it cannot read instead of papering over a hole. `/resume`
  switches between the conversations held in the current directory without restarting the app.
- **Themes** that downsample themselves to whatever the terminal can actually display, with an
  ASCII glyph set for terminals where box drawing comes out wrong.
- **A file tree** beside the shell, rooted at the session's directory and marking what just changed,
  fed by a `notify` watcher with a `globset` of what not to watch (`target/**`, `.git/**`, swap
  files) rather than by re-reading the project every frame.
- **Sessions to come back to**: `--continue` picks up the last conversation held in this directory,
  `--resume <id>` a named one, and `--branch` forks one into a new file so you can start again from a
  decision point. A session is only offered for the directory it was held in.
- **A pane per command**: a `bash` tool call runs on the engine's side and appears in a terminal pane
  of its own, where `Ctrl+C` interrupts the call the way it interrupts anything else. The engine owns
  the process; the pane owns the screen.
- **Completion that knows what it is completing**: `/` for commands, `@` for paths, and a command's
  own values after it — `/theme dr` offers `dracula`, `/prompt` offers your saved templates.
- **A mouse, as an addition to the keyboard**: a click focuses a pane, the wheel scrolls the one under
  the pointer, and dragging a split line resizes it. Every one of those has a key that does the same
  thing, because a terminal app is used by people whose hands are already on the keyboard.

## Install

The release carries a tarball per platform plus one `SHA256SUMS` covering all of them. Download the
pair, check the tarball against the checksum, and put the binary somewhere on your `PATH`:

```sh
gh release download --repo GBLMX/jmds --pattern 'jmds-*.tar.gz' --pattern SHA256SUMS
sha256sum -c SHA256SUMS
tar xzf jmds-*.tar.gz
install -m755 jmds ~/.local/bin/jmds
```

Without `gh`, the same files are on the release page. Today the matrix publishes one target,
`x86_64-unknown-linux-gnu` — adding another is a row in `.github/workflows/release.yml`.

## Build and run

```sh
cargo build --release
DEEPSEEK_API_KEY=sk-… ./target/release/jmds

# and, next time, from the same directory:
jmds --continue          # continue the latest conversation held here
jmds --resume <id>       # continue a named one
jmds <id>                # the same thing, for when you have the id in hand
jmds --branch [<id>]     # start a new conversation branched off one
jmds --keep <n>          # with --branch: how many messages of its history to keep
jmds --version           # which build this is
jmds --help
```

The log lives under `~/.config/jmds/logs/`, beside the sessions and the prompt templates: a log in the
working directory is a log in whatever project the app was started from, which is where it is least
wanted.

### A shell is what the `bash` tool is

A `bash` call is handed to `bash` on `PATH` — the tool is a shell, not an imitation of one, which is
why interrupting a call interrupts a process group and why the pane can be a real terminal. The tests
that exercise it therefore need a shell, and that makes them Unix tests: on Windows there is nothing
for the tool to run commands with, so those tests are `#[cfg(unix)]` rather than red. The Windows job
still builds the workspace and runs everything that needs no shell.

The key is read from the environment variable named in the config file, never from the file itself:
`~/.config/jmds/config.toml`, read on every start and written when `/theme`, `/glyphs` or `/model`
changes something — the write edits your document in place, so comments and order survive. A missing
key is not a crash: the app runs, and says so in the transcript when you ask something. `/key` says
the same thing on demand, including the line to put in your shell.

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
| `Alt+←` / `Alt+→` | move the focused pane's split line sideways |
| `Alt+↑` / `Alt+↓` | move it up and down |
| `/` then `Tab` | complete a command; `/theme `, `/model ` and `/prompt ` complete their values |
| `@` then `Tab` | complete a path |
| click / wheel / drag | focus a pane / scroll the one under the pointer / move a split line |

Keys are offered to the focused pane first, except the two that quit and close: a pane can refuse a
key, but it cannot trap you.

## Commands

Typed in the input line, starting with a slash:

| Command | What it does |
|---|---|
| `/help` | what the keys do |
| `/clear` | empty the transcript; the session file stays |
| `/model [<name>]` | which model answers: alone it says which one and what else there is; with a name it switches, and writes the default a *new* session starts from — a continued session keeps the model its history came from |
| `/key` | where the key is read from, whether this process has one, and the line that sets it |
| `/resume [<id>]` | continue another conversation held in this directory; alone, it lists them |
| `/theme <name>` | switch colours: `terminal`, `default`, `dracula`, `nord`, `gruvbox`, `tokyo-night`, `catppuccin`, `one-dark` |
| `/prompt <name>` | start the prompt file from a template in `~/.config/jmds/prompts/` |
| `/glyphs unicode\|ascii` | which glyph set to draw with |
| `/quit` | leave |

The prompt file the editor pane holds is `~/.config/jmds/prompts/prompt.md`: a frontmatter with the
path to change and one line of instruction, then the body that gets sent. `/prompt` writes a saved
template into it, and refuses when the editor has unsaved work.

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
