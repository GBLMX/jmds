# Credits

jmds is a personal project, and it stands on three others. This file says exactly what came from
where, and — because the difference matters both legally and for anyone reading the code — which
parts are **ported code** and which are **learned ideas** with the implementation written here.

Nothing below was copied without its licence allowing it. The ported files are listed with their
paths so the claim can be checked rather than believed; [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES)
carries the notices those licences require.

---

## Ported code

### [GBLMX/pigma](https://github.com/GBLMX/pigma) — Apache-2.0

The closest relative: a Rust terminal app by the same author, and the source of the parts of jmds
that are about *talking to a terminal* rather than about DeepSeek.

| Here | There | What changed |
|---|---|---|
| `crates/jmds-tui/src/terminal/*` | `src/utils/terminal/*` | Adapted: the capability probes, the colour maths (`rgb_to_256`, `rgb_to_16`, `ANSI_16`), the OSC 11 background probe with its per-platform split, and the escape-sequence emitters. The theme layer it was written for did not exist yet on this side. |
| `crates/jmds-core/src/config/migrate.rs`, parts of `config.rs` | `src/config/*` | Adapted: the migration mechanism — edit the user's document in place, decide by a serde probe rather than by a version number's arithmetic. |
| `.github/workflows/ci.yml`, `release.yml`, `cliff.toml`, `.githooks/pre-commit`, `.cargo/config.toml` | the same files | Adapted: the project scaffolding. Trimmed to what this workspace needs (no pages deploy, no packaging). |
| the hex values in `crates/jmds-tui/src/theme.rs` | `src/config/theme.rs` | Taken as data: the built-in palettes (`default`, `dracula`, `nord`, `gruvbox`, `tokyo-night`, `catppuccin`, `one-dark`) are pigma's values, because a theme is a set of numbers someone already checked against a real terminal. |

Apache-2.0 obliges us to keep the notices and to say what was changed; both are in
[`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES).

## Learned ideas — implementation written here

### [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) — MIT

An agent harness in TypeScript whose design was read closely and then *reimplemented* in Rust. No
file was copied; what was taken is a set of decisions about how a coding agent should behave.

- **What each tool's output policy is** → `crates/jmds-core/src/tools/truncate.rs`. Two shapes,
  because two kinds of output fail in opposite directions: a file is read from the top and the
  model is told which line to ask for next; a command's output is read from the bottom, because
  errors are at the end. The rule both obey — never a partial line, never silence — is the one
  thing that matters about truncation.
- **The timeout table** → `crates/jmds-core/src/tools/bash.rs`. Default 300 s, floor 1 s, ceiling
  3600 s, `0` meaning "no deadline", and a clamp the model is *told about* rather than a silent
  shortening.
- **Lifting a leading `cd <dir> &&` out of a command** → the same file. Models write that
  constantly, and running it as written would make the reported working directory a lie.
- **A 512-byte cap per line, a 256 KiB in-memory budget, and the rest mirrored to an artifact the
  answer names** → the same file.
- **The role vocabulary a theme names** (`accent`, `border`, `muted`, `dim`, `success`, `error`,
  `toolOutput`, …) → `crates/jmds-tui/src/theme.rs`, shrunk to the fifteen roles this app draws
  with, and the `unicode | nerd | ascii` axis for glyph sets.
- **Conversation-pane policies** → `crates/jmds-tui/src/pane/chat.rs`: fold long reasoning (and
  never the answer), follow the newest output only while the view is already at the bottom, and
  treat scrolling up as a request to read rather than as a position to be corrected.
- **Always-available keys** → `crates/jmds-tui/src/app.rs`: a pane may refuse a key but not the
  ones that quit or close it, and everything else the focused pane gets first refusal on.
- **The DeepSeek compatibility rules** → `crates/jmds-api/src/message.rs` and
  `crates/jmds-core/src/agent.rs`: replay `reasoning_content` on every assistant turn (even when
  it is empty), always send `max_tokens`, never send `tool_choice`, and normalise the three shapes
  the usage report arrives in.
- **A retry must not happen once the answer has started** → `crates/jmds-api/src/client.rs`.
- **The shape of an append-only session file, and the rule that recovery stops at the first line it
  cannot read** → `crates/jmds-core/src/session.rs`. The order of those lines is the contract: it is
  what gets replayed to the provider, and the prefix cache only hits if the prefix is what it was.
- **The turn loop's bookkeeping** → `crates/jmds-core/src/agent.rs`: announce a tool call only once
  it has been reassembled, run the calls in the order asked, and put one `tool` message back per
  result.

### [ccch1mneyyy/dsh-TUI](https://github.com/ccch1mneyyy/dsh-TUI) — MIT

A TypeScript TUI for DeepSeek. Read for two things, neither of them code:

- **The grammar a colour may be written in** (`#rgb`, `#rrggbb`, `rgb(r,g,b)`, `ansi256(n)`, the
  sixteen `ansi:` names) and the rule that a theme name from a config file is checked before it is
  used as a file name → `crates/jmds-tui/src/theme.rs`. `#rrggbbaa` is not accepted here: ratatui
  has no alpha channel.
- **The pricing table and its time windows** (`crates/jmds-core/src/…`, not yet wired into a status
  line): DeepSeek's per-model prices as `[idle, peak]` pairs in CNY per million tokens, with peak
  meaning Beijing time, Monday to Friday, 09:00–12:00 and 14:00–18:00 — and the observation that a
  session's tokens have to be counted *per bucket*, because a session that spans both windows is
  not priced at one rate.

## Read and deliberately not taken

Naming these matters as much as naming the borrows, because each one is a real temptation:

- **Five coexisting edit formats** and a growth in tool count that follows from them. jmds has one
  edit format, and the tool table is four tools wide on purpose — every tool definition is prompt
  prefix.
- **Vendoring a shell** (a forked shell core plus ported utilities). jmds runs the real one.
- **Compaction as a platform** (six triggers, four methods, transcript frames shipped to the
  provider as images). jmds truncates, and says so.
- **A hand-written TUI engine.** ratatui owns the cell buffer and the diff; the policies above are
  what was worth learning from a project that wrote its own.
- **The private-use-area glyph protocol and its bundled font outlines.** Without real codepoints
  the fallback would be tofu, which is worse than ASCII; jmds ships a `unicode` and an `ascii` set
  and no invented characters.
- **Screenshots and image assets.** This is a cell-based renderer.
- **Per-session colour derivation and animation effects as a subsystem.** jmds has one session per
  process, and its effects (`crates/jmds-tui/src/effects.rs`) are pure functions of a frame counter
  — a gradient, a travelling highlight, a spinner — drawn with ratatui's own types and nothing more.
