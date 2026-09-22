//! How much of an answer a tool shows, and how it says so.
//!
//! Two shapes, because two kinds of output fail in opposite directions. A file is read from the
//! top — what comes next is below, so [`head`] keeps the beginning and tells the model which line
//! to ask for next. A command's output is read from the bottom — errors are at the end, the
//! interesting part of a build log is the last screen — so [`tail`] keeps the end and reports how
//! much it dropped.
//!
//! The rule both shapes obey: **never a half line, and never silence**. A truncated read that
//! returns a partial line hands the model a broken token to reason about; a truncated read that
//! says nothing hands it a file it believes is short.

/// Lines a tool shows before it starts truncating. Big enough for a real source file in one call,
/// small enough that four tools running at once cannot flood a context window.
pub const MAX_LINES: usize = 2000;

/// Bytes a tool shows before it starts truncating — the other half of the same bound, because one
/// enormous line is not covered by a line count.
pub const MAX_BYTES: usize = 50 * 1024;

/// How much of a single line a command's output may show.
///
/// A build log or a minified file can have one line of megabytes; without a per-line cap a single
/// line can fill the whole budget and the hundred useful lines after it are never shown. Only the
/// tail shape caps columns: a *read* refuses a first line it cannot show whole (see
/// [`head`]'s `first_line_overflows`) because half a source line is worse than an instruction.
pub const MAX_COLUMN: usize = 512;

/// What one truncation did, and what the caller has to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    /// The text to show. Whole lines only.
    pub text: String,
    /// The first line of the input that is shown, 1-based.
    pub first_line: usize,
    /// The last line of the input that is shown, 1-based and inclusive. Zero when nothing is
    /// shown.
    pub last_line: usize,
    /// How many lines the input had.
    pub total_lines: usize,
    /// True when the text is all of it.
    pub complete: bool,
    /// The first line of the input is longer than the byte budget on its own, so nothing useful can
    /// be shown from here. Only [`head`] reports this: a long first line is no reason to hide a
    /// command's last lines.
    pub first_line_overflows: bool,
}

impl Truncated {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The `offset` to ask for next — when there is more *below* what was shown.
    ///
    /// 1-based, to match the `offset` a read takes. It is a method rather than arithmetic the
    /// caller remembers for two reasons: a [`tail`] shows the end of the input, so there is
    /// nothing below it to ask for (what was dropped is above); and a read whose first line did
    /// not fit at all has to change the question instead of stepping forward.
    pub fn next_offset(&self) -> Option<usize> {
        (!self.first_line_overflows && self.last_line < self.total_lines)
            .then(|| self.last_line + 1)
    }
}

/// Keep the beginning: what a read of a file wants.
///
/// `from` is a 1-based line number to start at (the `offset` a previous read told the model to
/// use). The budget is spent line by line: the first line that would cross either bound ends the
/// text, so the result never contains half a line.
pub fn head(text: &str, from: usize, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let start = from.saturating_sub(1).min(total_lines);

    // A single line that cannot fit is reported as such rather than shown cut in half: whoever
    // asked has to change the question (a byte range, a smaller piece) instead of reading a
    // fragment that looks complete.
    if let Some(first) = lines.get(start)
        && first.len() > max_bytes
    {
        return Truncated {
            text: String::new(),
            first_line: start + 1,
            last_line: start,
            total_lines,
            complete: false,
            first_line_overflows: true,
        };
    }

    let mut out = String::new();
    let mut end = start;
    for line in lines.iter().skip(start).take(max_lines) {
        let extra = line.len() + usize::from(!out.is_empty());
        if out.len() + extra > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        end += 1;
    }
    if out.is_empty() && start < total_lines {
        // The budget was smaller than one line's worth (a caller passing a tiny max_bytes). Show
        // the line rather than nothing: an empty answer to "show me this line" is worse than a
        // long line.
        out.push_str(lines[start]);
        end = start + 1;
    }

    Truncated {
        text: out,
        first_line: start + 1,
        last_line: end,
        total_lines,
        complete: end >= total_lines,
        first_line_overflows: false,
    }
}

/// Keep the end: what a command's output wants.
pub fn tail(text: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    let mut chosen: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if chosen.len() >= max_lines {
            break;
        }
        let extra = line.len() + usize::from(!chosen.is_empty());
        if bytes + extra > max_bytes {
            break;
        }
        bytes += extra;
        chosen.push(line);
    }
    chosen.reverse();

    let first_line_overflows = chosen.is_empty() && !lines.is_empty() && lines[0].len() > max_bytes;
    let start = total_lines - chosen.len();
    Truncated {
        text: chosen.join("\n"),
        first_line: start + 1,
        last_line: total_lines,
        total_lines,
        complete: start == 0,
        first_line_overflows,
    }
}

/// Cut every line to `cap` bytes, marking where it was cut.
///
/// The mark matters: a silently shortened line reads as a complete one.
pub fn cap_columns(text: &str, cap: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if line.len() <= cap {
            out.push_str(line);
            continue;
        }
        // Not on a char boundary necessarily: cut on the last boundary that fits, so the result
        // stays valid UTF-8 rather than being a byte slice of a multi-byte character.
        let mut end = cap;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&line[..end]);
        out.push_str(&format!("…[+{} bytes]", line.len() - end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_short_file_comes_back_whole() {
        let text = numbered(3);
        let out = head(&text, 1, MAX_LINES, MAX_BYTES);
        assert_eq!(out.text, text);
        assert!(out.complete);
        assert_eq!((out.first_line, out.last_line), (1, 3));
        assert_eq!(out.total_lines, 3);
    }

    #[test]
    fn a_long_file_is_cut_at_a_line_boundary_and_says_where_to_continue() {
        let text = numbered(10);
        let out = head(&text, 1, 3, MAX_BYTES);
        assert_eq!(out.text, "line 1\nline 2\nline 3");
        assert!(!out.complete);
        assert_eq!((out.first_line, out.last_line), (1, 3));
        assert_eq!(out.total_lines, 10);
        // The line to ask for next is the one after the last shown.
        assert_eq!(out.next_offset(), Some(4));
    }

    #[test]
    fn an_offset_starts_where_the_last_answer_stopped() {
        let text = numbered(10);
        let out = head(&text, 4, 3, MAX_BYTES);
        assert_eq!(out.text, "line 4\nline 5\nline 6");
        assert_eq!((out.first_line, out.last_line), (4, 6));
        assert_eq!(out.next_offset(), Some(7));
    }

    #[test]
    fn an_offset_past_the_end_is_an_empty_answer_and_not_an_error() {
        let text = numbered(3);
        let out = head(&text, 99, MAX_LINES, MAX_BYTES);
        assert!(out.is_empty());
        assert!(out.complete);
        assert_eq!(out.total_lines, 3);
        assert_eq!(
            out.last_line, 3,
            "nothing is shown, but the input still had three lines"
        );
    }

    #[test]
    fn the_byte_bound_can_cut_before_the_line_bound_does() {
        let text = "aaaa\nbbbb\ncccc\ndddd";
        let out = head(text, 1, MAX_LINES, 11);
        assert_eq!(
            out.text, "aaaa\nbbbb",
            "two lines and a newline fit in 11 bytes"
        );
        assert!(!out.complete);
    }

    #[test]
    fn a_first_line_longer_than_the_whole_budget_is_reported_instead_of_cut_in_half() {
        // This is the case a read has to answer with "use bash" rather than with a fragment: half
        // a line looks like a complete line to whoever reads it next.
        let text = format!("{}\nshort", "x".repeat(1000));
        let out = head(&text, 1, MAX_LINES, 100);
        assert!(out.first_line_overflows);
        assert!(out.is_empty());
        assert_eq!(out.total_lines, 2);
    }

    #[test]
    fn the_tail_keeps_the_end_where_the_errors_are() {
        let text = numbered(10);
        let out = tail(&text, 3, MAX_BYTES);
        assert_eq!(out.text, "line 8\nline 9\nline 10");
        assert!(!out.complete);
        assert_eq!((out.first_line, out.last_line), (8, 10));
        assert_eq!(out.total_lines, 10);
        assert_eq!(
            out.next_offset(),
            None,
            "the tail is the end: nothing to ask for next"
        );
    }

    #[test]
    fn a_tail_that_fits_is_complete() {
        let text = numbered(3);
        let out = tail(&text, 10, MAX_BYTES);
        assert_eq!(out.text, text);
        assert!(out.complete);
        assert_eq!(
            (out.first_line, out.last_line),
            (1, 3),
            "everything is shown, so it starts at the top"
        );
    }

    #[test]
    fn an_empty_input_is_not_a_special_case_for_either_shape() {
        for out in [
            head("", 1, MAX_LINES, MAX_BYTES),
            tail("", MAX_LINES, MAX_BYTES),
        ] {
            assert!(out.is_empty());
            assert_eq!(out.total_lines, 0);
        }
    }

    #[test]
    fn a_very_long_line_is_cut_with_a_mark_and_the_rest_is_left_alone() {
        let long = "x".repeat(1000);
        let text = format!("short\n{long}\nalso short");
        let capped = cap_columns(&text, 100);
        let lines: Vec<&str> = capped.split('\n').collect();
        assert_eq!(lines[0], "short");
        assert_eq!(lines[2], "also short");
        assert!(
            lines[1].starts_with(&"x".repeat(100)),
            "{:?}",
            &lines[1][..80]
        );
        assert!(lines[1].ends_with("…[+900 bytes]"), "{}", &lines[1][90..]);
    }

    #[test]
    fn cutting_a_column_never_splits_a_character() {
        // Every char is three bytes, so a cap of 5 lands mid-character.
        let text = "①②③";
        let capped = cap_columns(text, 5);
        assert!(capped.starts_with('①'), "{capped}");
        assert_eq!(capped.chars().filter(|c| *c == '①').count(), 1);
    }

    #[test]
    fn neither_shape_ever_returns_a_partial_line() {
        // Every line of the output must be one of the input's lines — that is what "whole lines
        // only" means, and it is the property the model depends on.
        let text = numbered(200);
        for max_bytes in [1, 7, 8, 20, 100, 1000] {
            let out = head(&text, 1, MAX_LINES, max_bytes);
            for line in out.text.lines() {
                assert!(line.starts_with("line "), "cut line: {line:?}");
            }
            let out = tail(&text, MAX_LINES, max_bytes);
            for line in out.text.lines() {
                assert!(line.starts_with("line "), "cut line: {line:?}");
            }
        }
    }
}
