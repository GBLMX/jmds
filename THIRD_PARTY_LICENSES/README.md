# Third-party licences

jmds is MIT ([`LICENSE`](../LICENSE)). It also contains code ported from another project, whose
licence asks for two things beyond attribution: that its text travels with the distribution, and
that files which were changed say so. This directory is both.

[`CREDITS.md`](../CREDITS.md) has the full accounting — what was ported, what was learned as an
idea, and what was deliberately left out. This file is the licence side of it.

## GBLMX/pigma — Apache License 2.0

Full text: [`pigma-APACHE-2.0.txt`](pigma-APACHE-2.0.txt), a copy of the licence as distributed with
that project.

### Files derived from it

| File | Changed how |
|---|---|
| `crates/jmds-tui/src/terminal/mod.rs` | Trimmed to the four subjects this app has; the re-exports were reorganised, and the colour maths became public because the theme layer now calls it. |
| `crates/jmds-tui/src/terminal/capability.rs` | Kept the probes; dropped the ones for features this app has no use for. |
| `crates/jmds-tui/src/terminal/color.rs` | Kept the tables and the maths; `color_luminance` and `background_from_luminance` are now `pub` so the theme layer can classify a palette's own background. |
| `crates/jmds-tui/src/terminal/sequences.rs` | Kept the sequences (bracketed paste, the kitty keyboard flag, synchronised updates, notifications); the notification path keeps its two OSC forms and the payload cap. |
| `crates/jmds-tui/src/terminal/background.rs` (and `background/unix.rs`, `background/windows.rs`) | Kept the probe and its per-platform split. |
| `crates/jmds-core/src/config.rs`, `crates/jmds-core/src/config/migrate.rs` | The migration mechanism, adapted to this file's fields: edit the user's document in place, decide with a serde probe. |
| `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `cliff.toml`, `.githooks/pre-commit`, `.cargo/config.toml` | The project scaffolding, trimmed to this workspace (no pages deploy, no packaging). |
| the built-in palettes' colour values in `crates/jmds-tui/src/theme.rs` | Taken as data and reused unchanged; the theme *structure* around them is written for ratatui's types rather than for that project's. |

Every file in that table carries a line saying so at the top, which is the other half of what
Apache-2.0 §4(b) asks for.

## MIT projects — credited, nothing copied

Two projects were read for their ideas, and no file from either was copied. They are credited in
[`CREDITS.md`](../CREDITS.md) because crediting an idea costs nothing and hiding where it came from
costs the reader:

- **can1357/oh-my-pi** (MIT) — tool output policies, the DeepSeek compatibility rules, the theme
  role vocabulary, and several TUI policies. Reimplemented in Rust.
- **ccch1mneyyy/dsh-TUI** (MIT) — the colour grammar a theme file may use, and the DeepSeek pricing
  table with its peak/idle windows.
