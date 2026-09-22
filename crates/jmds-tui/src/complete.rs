//! Completions for the input line: what token the caret is in, what could follow, and what
//! accepting one does to the text.
//!
//! The shape is borrowed from oh-my-pi's composer — one input surface, with prefixes recognised
//! from the text rather than a mode you switch into — and shrunk to the two prefixes this app has:
//!
//! - `/` **at the start of the line** opens a command. A `/` anywhere else is a path, which is why
//!   the *position* decides rather than the character.
//! - `@` opens a path, optionally quoted. `@"a file with spaces` is one token: the quote is tracked
//!   rather than guessed, because a space inside a path is the normal case, not the exception.
//!
//! Three decisions worth naming, each of which is a bug the other way round:
//!
//! - **The token is found from the caret backwards, and the caret is the authority.** Completing the
//!   *last* token in the line would be easier and would be wrong the moment someone goes back to
//!   edit the middle of a line they had already typed.
//! - **Accepting a directory keeps the token open.** Tab on `@src` gives `@src/` and the menu stays
//!   up, because the next thing anyone does with a directory is descend into it. The rule lives in
//!   one place — [`Completion::accept`] — so Tab and Enter cannot drift apart, which is the classic
//!   way a completion menu becomes infuriating.
//! - **Nothing here touches the file system.** Candidates come from a [`Source`], so matching,
//!   ranking and the text splicing are testable without a repository, and the caller decides what
//!   `@` means on this machine.

/// What a token is introduced by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    /// A command: `/` at the start of the line.
    Command,
    /// A command's argument: the word after `/command `.
    ///
    /// Which values are any good is the command's business, not this engine's; the engine's job is
    /// only to say that the caret is in the position where a value goes.
    Argument,
    /// A path: `@`, optionally followed by a quote.
    Path,
}

/// The token the caret is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Where the token starts, in characters, including its prefix.
    pub start: usize,
    /// Where it ends: the caret, since a token is only ever completed up to the caret.
    pub end: usize,
    pub prefix: Prefix,
    /// What has been typed after the prefix, with any opening quote removed.
    pub query: String,
    /// Whether the query was quoted, so acceptance knows whether to close the quote.
    pub quoted: bool,
}

/// One candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What the menu shows.
    pub label: String,
    /// What is written into the line.
    pub insert: String,
    /// A line of explanation, shown beside the label.
    pub detail: String,
    /// Whether accepting this leaves the token open for more: a directory, or a command that takes
    /// an argument.
    pub continues: bool,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            insert: label.clone(),
            label,
            detail: String::new(),
            continues: false,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Leaves the token open after acceptance: `<insert>/` for a directory, and the menu stays up.
    pub fn continuing(mut self) -> Self {
        self.continues = true;
        self
    }
}

/// Where candidates come from. The engine never looks at the file system itself.
pub trait Source {
    fn candidates(&self, prefix: Prefix, query: &str) -> Vec<Item>;

    /// The values a command takes, for the caret sitting in its argument.
    ///
    /// Asked separately — and by name — because the engine can tell the caret is in an argument but
    /// not which command put it there: that is a fact about the line, which the caller has and this
    /// engine deliberately does not.
    fn argument(&self, _command: &str, _query: &str) -> Vec<Item> {
        Vec::new()
    }
}

/// The menu: what is offered, and what is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The token being completed.
    pub token: Token,
    pub items: Vec<Item>,
    pub selected: usize,
}

/// Characters that end a token: what a shell would split on, plus the quote.
const DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

/// How many candidates a menu shows at once. A menu taller than the space above the input is a menu
/// nobody reads.
pub const VISIBLE: usize = 8;

impl Completion {
    /// Work out what, if anything, the caret is in the middle of.
    ///
    /// `caret` is in characters, like everything else the input line does, so a line with wide
    /// characters counts what a person sees rather than what the bytes happen to be.
    pub fn at(text: &str, caret: usize, source: &impl Source) -> Option<Self> {
        let token = token_at(text, caret)?;
        let mut items = match token.prefix {
            Prefix::Argument => match command_word(text) {
                Some(command) => source.argument(command, &token.query),
                None => Vec::new(),
            },
            prefix => source.candidates(prefix, &token.query),
        };
        rank(&mut items, &token.query);
        if items.is_empty() {
            return None;
        }
        Some(Self {
            token,
            items,
            selected: 0,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }

    /// Move the selection, wrapping at both ends: a menu that stops at the last entry makes the
    /// first one unreachable from the bottom, which nobody expects.
    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let count = self.items.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }

    /// Whether taking the selection would leave the line exactly as it is.
    ///
    /// True when the one candidate is what has already been typed: a menu whose acceptance does
    /// nothing has nothing to offer, and leaving it up would swallow the next `Tab` for no reason.
    pub fn is_noop(&self, text: &str, caret: usize) -> bool {
        match self.selected_item() {
            Some(item) => self.accept(text, item) == (text.to_string(), caret),
            None => false,
        }
    }

    /// Write an item into `text`, replacing the token up to the caret.
    ///
    /// Returns the new text and where the caret belongs. A directory keeps the token open — the
    /// prefix comes back and a quote stays open — so the next keystroke continues the path.
    pub fn accept(&self, text: &str, item: &Item) -> (String, usize) {
        let characters: Vec<char> = text.chars().collect();
        let before: Vec<char> = characters[..self.token.start.min(characters.len())].to_vec();
        // What is after the caret is the user's: accepting a completion is not a reason to eat it.
        let after: Vec<char> = characters[self.token.end.min(characters.len())..].to_vec();

        let sigil = match self.token.prefix {
            Prefix::Command => "/",
            // An argument has no sigil: what is being replaced is a bare word.
            Prefix::Argument => "",
            // The `@` is kept: it is what makes the line a path, and the quote only collects spaces.
            Prefix::Path if self.token.quoted => "@\"",
            Prefix::Path => "@",
        };
        let mut inserted: Vec<char> = sigil.chars().collect();
        inserted.extend(item.insert.chars());
        if item.continues {
            if !item.insert.ends_with('/') {
                inserted.push('/');
            }
        } else if self.token.prefix == Prefix::Path && self.token.quoted {
            inserted.push('"');
        }

        let caret = before.len() + inserted.len();
        let mut out = before;
        out.extend(inserted);
        out.extend(after);
        (out.into_iter().collect(), caret)
    }
}

/// The token the caret is inside, or `None` when it is not in one.
///
/// Where a command's argument starts, if the caret is in it.
///
/// `None` for everything else: a line without a command, a caret still inside the command word, and a
/// command whose word is not over yet. The space is what decides between "typing a command" and
/// "typing its argument".
fn argument_position(before: &[char]) -> Option<usize> {
    let leading = before.iter().take_while(|c| c.is_whitespace()).count();
    if before.get(leading) != Some(&'/') {
        return None;
    }
    let command_end = leading
        + 1
        + before[leading + 1..]
            .iter()
            .take_while(|c| !c.is_whitespace())
            .count();
    if before.get(command_end) != Some(&' ') {
        return None;
    }
    // Skip the spaces between: the argument is the word the caret is in, wherever it starts.
    let start = command_end
        + before[command_end..]
            .iter()
            .take_while(|c| c.is_whitespace())
            .count();
    Some(start)
}

/// The command word of a line, without its slash — the word the caret is completing if it sits in the
/// argument position.
fn command_word(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let body = trimmed.strip_prefix('/')?;
    let name = body.split_whitespace().next()?;
    let rest = &body[name.len()..];
    // Only when the word is over: `--` and `/theme` with nothing after it are not arguments yet.
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then_some(name)
}

pub fn token_at(text: &str, caret: usize) -> Option<Token> {
    let characters: Vec<char> = text.chars().collect();
    let caret = caret.min(characters.len());
    let before = &characters[..caret];

    let leading = before.iter().take_while(|c| c.is_whitespace()).count();
    if before.get(leading) == Some(&'/') {
        let query: String = before[leading + 1..]
            .iter()
            .take_while(|c| !c.is_whitespace())
            .collect();
        // The caret is still inside the command word: after its space, the word is done.
        if leading + 1 + query.chars().count() == caret {
            return Some(Token {
                start: leading,
                end: caret,
                prefix: Prefix::Command,
                query,
                quoted: false,
            });
        }
    }

    // A command's argument: `/theme dr` with the caret in `dr`. A command word with the caret still
    // in it is a command being typed, not an argument, which is what the space decides.
    if let Some(start) = argument_position(before) {
        return Some(Token {
            start,
            prefix: Prefix::Argument,
            query: before[start..].iter().collect(),
            end: caret,
            quoted: false,
        });
    }

    // Whether the caret sits inside an unclosed quote decides what a delimiter means: inside one, a
    // space is part of the path; outside, it ends the token. Counting the quotes is how a terminal
    // reads a shell line, and it is why `@"a file` is one token while `@a file` is two.
    let quoted_ahead = before.iter().filter(|character| **character == '"').count() % 2 == 1;

    // Walk back to the `@` that opens the token.
    let mut index = caret;
    while index > 0 {
        index -= 1;
        let character = before[index];
        if character == '@' {
            let quoted = before.get(index + 1) == Some(&'"');
            let from = index + 1 + usize::from(quoted);
            return Some(Token {
                start: index,
                end: caret,
                prefix: Prefix::Path,
                query: before[from.min(caret)..].iter().collect(),
                quoted,
            });
        }
        if !quoted_ahead && DELIMITERS.contains(&character) {
            return None;
        }
    }
    None
}

/// Put the best candidates first.
///
/// Three tiers, in the order a person expects: an exact prefix, a prefix that starts a later word,
/// then a subsequence ("wig" ~ "skill:wig"). Within a tier, shorter labels first — the thing you
/// meant is usually the thing with the least to say — and ties keep the source's order, so the menu
/// does not jump about between keystrokes.
pub fn rank(items: &mut [Item], query: &str) {
    if query.is_empty() {
        return;
    }
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, usize)> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let label = item.label.to_lowercase();
            let tier = if label.starts_with(&query) {
                0
            } else if label
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word.starts_with(&query))
            {
                1
            } else if subsequence_match(&query, &label) {
                2
            } else {
                3
            };
            (tier, item.label.chars().count(), index)
        })
        .collect();
    scored.sort_unstable();

    let order: Vec<usize> = scored.iter().map(|(_, _, index)| *index).collect();
    let taken: Vec<Item> = order.iter().map(|index| items[*index].clone()).collect();
    for (slot, item) in items.iter_mut().zip(taken) {
        *slot = item;
    }
}

/// Whether `query` appears in `target` in order, without needing to be contiguous.
pub fn subsequence_match(query: &str, target: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut wanted = query.chars();
    let mut next = wanted.next();
    for character in target.chars() {
        if Some(character) == next {
            next = wanted.next();
            if next.is_none() {
                return true;
            }
        }
    }
    next.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source with a fixed list: the engine's own behaviour is what is under test.
    struct Fixed(Vec<Item>);

    impl Source for Fixed {
        fn candidates(&self, _prefix: Prefix, _query: &str) -> Vec<Item> {
            self.0.clone()
        }

        /// The same list for arguments: this double exists to answer "what is offered", and which
        /// question was asked is the engine's business, not the double's.
        fn argument(&self, _command: &str, _query: &str) -> Vec<Item> {
            self.0.clone()
        }
    }

    fn source(names: &[&str]) -> Fixed {
        Fixed(names.iter().map(|name| Item::new(*name)).collect())
    }

    fn accept(text: &str, caret: usize, item: Item, source: &impl Source) -> (String, usize) {
        let completion = Completion::at(text, caret, source).expect("a menu");
        completion.accept(text, &item)
    }

    #[test]
    fn a_slash_at_the_start_of_the_line_is_a_command() {
        let token = token_at("/the", 4).unwrap();
        assert_eq!(token.prefix, Prefix::Command);
        assert_eq!(token.query, "the");
        assert_eq!((token.start, token.end), (0, 4));

        // Leading whitespace is still the start of the line.
        let token = token_at("   /the", 7).unwrap();
        assert_eq!(token.prefix, Prefix::Command);
        assert_eq!(token.start, 3, "the token starts at the slash");
        assert_eq!(token.query, "the");

        // Once the command's word is finished, the caret is in its argument instead — which is a
        // different question with a different answer, and the test below is where that lives.
        assert_eq!(
            token_at("/theme ", 7).expect("参数的位置").prefix,
            Prefix::Argument
        );
        // And a slash anywhere else is a path that nobody asked to complete.
        assert!(token_at("look at /usr", 12).is_none());
    }

    #[test]
    fn an_at_sign_opens_a_path_token() {
        let token = token_at("read @src/ma", 12).unwrap();
        assert_eq!(token.prefix, Prefix::Path);
        assert_eq!(token.query, "src/ma");
        assert!(!token.quoted);
        assert_eq!(token.start, 5, "the token starts at the @");
        assert_eq!(token.end, 12);
    }

    #[test]
    fn a_quote_keeps_a_space_inside_the_token() {
        let token = token_at("read @\"a file", 13).unwrap();
        assert_eq!(token.prefix, Prefix::Path);
        assert!(token.quoted);
        assert_eq!(token.query, "a file");

        // Unquoted, the space ends the token: `@a` is not what the caret is in.
        assert!(token_at("read @a file", 11).is_none());
    }

    #[test]
    fn the_caret_decides_which_token_is_completed() {
        // Not the last token in the line: the one the caret is in.
        let text = "read @src/lib.rs and @Cargo.toml";
        let token = token_at(text, 16).unwrap();
        assert_eq!(token.query, "src/lib.rs");
        assert_eq!(token.end, 16, "the token ends at the caret");
        // Two characters earlier the query is what has been typed *up to* the caret: completing a
        // word is not a reason to notice the characters after it.
        assert_eq!(token_at(text, 14).unwrap().query, "src/lib.");
    }

    #[test]
    fn a_caret_outside_any_token_completes_nothing() {
        assert!(token_at("just some words", 9).is_none());
        assert!(token_at("", 0).is_none());
        assert!(
            token_at("@", 1).is_some(),
            "an empty query is the whole list"
        );
    }

    #[test]
    fn candidates_are_ranked_prefix_first_then_shorter_then_subsequence() {
        let mut items: Vec<Item> = ["theme", "the", "other", "t-h-e", "skill:theme"]
            .iter()
            .map(|name| Item::new(*name))
            .collect();
        rank(&mut items, "the");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels[0], "the", "prefix, and the shortest of them");
        assert_eq!(labels[1], "theme", "also a prefix");
        assert_eq!(
            labels[2], "skill:theme",
            "a prefix starting a later word comes before a bare subsequence"
        );
        // The last two are both subsequence matches — "other" has t-h-e in order — so which of them
        // lands first is a tie. The contract is that ties keep the source's order, not that a
        // particular one wins, and pinning that would be pinning an accident.
        assert!(labels[2..].contains(&"t-h-e"), "{labels:?}");
        assert!(labels[2..].contains(&"other"), "{labels:?}");
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn an_empty_query_keeps_the_sources_order() {
        let mut items: Vec<Item> = ["three", "one", "two"]
            .iter()
            .map(|name| Item::new(*name))
            .collect();
        rank(&mut items, "");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            labels,
            ["three", "one", "two"],
            "the menu must not jump about"
        );
    }

    #[test]
    fn the_selection_wraps_at_both_ends() {
        let mut completion = Completion::at("/", 1, &source(&["a", "b", "c"])).unwrap();
        assert_eq!(completion.selected, 0);
        completion.move_selection(-1);
        assert_eq!(completion.selected, 2, "up from the top is the bottom");
        completion.move_selection(1);
        assert_eq!(completion.selected, 0);
    }

    #[test]
    fn the_caret_in_a_commands_argument_is_an_argument() {
        let token = token_at("/theme dr", 9).expect("参数里的光标");
        assert_eq!(token.prefix, Prefix::Argument);
        assert_eq!(token.query, "dr");
        assert_eq!(token.start, 7);

        // The space is the boundary, so an argument that is still empty is still an argument.
        let token = token_at("/theme ", 7).expect("刚敲完空格");
        assert_eq!(token.prefix, Prefix::Argument);
        assert_eq!(token.query, "");
        // A caret still inside the command word is a command being typed.
        assert_eq!(
            token_at("/theme", 6).expect("还在命令里").prefix,
            Prefix::Command
        );
        // And prose is neither.
        assert_eq!(token_at("just words", 5), None);
    }

    #[test]
    fn accepting_an_argument_replaces_the_word_without_a_sigil() {
        let items = Fixed(vec![Item::new("dracula")]);
        let (text, caret) = accept("/theme dr", 9, items.0[0].clone(), &items);
        assert_eq!(text, "/theme dracula");
        assert_eq!(caret, 14, "光标落在插入的东西之后");
    }

    #[test]
    fn accepting_a_command_replaces_only_the_word_being_typed() {
        let items = Fixed(vec![Item::new("theme").with_detail("switch the colours")]);
        let (text, caret) = accept("/the", 4, items.0[0].clone(), &items);
        assert_eq!(text, "/theme");
        assert_eq!(caret, 6, "the caret lands after what was inserted");

        // What comes after the caret is left exactly as it was.
        let (text, _) = accept("/the and more", 4, items.0[0].clone(), &items);
        assert_eq!(text, "/theme and more");
    }

    #[test]
    fn accepting_a_directory_keeps_the_token_open() {
        let items = Fixed(vec![Item::new("src").continuing()]);
        let (text, caret) = accept("read @sr", 8, items.0[0].clone(), &items);
        assert_eq!(text, "read @src/", "a directory keeps the path going");
        assert_eq!(caret, 10, "so the next keystroke continues the path");

        // The quote, if there was one, stays open for the same reason.
        let (text, _) = accept("read @\"sr", 9, items.0[0].clone(), &items);
        assert_eq!(
            text, "read @\"src/",
            "the quote is not closed on a directory"
        );
    }

    #[test]
    fn accepting_a_file_closes_a_quote_it_opened() {
        let items = source(&["Cargo.toml"]);
        let (text, caret) = accept("read @\"Car", 11, Item::new("Cargo.toml"), &items);
        assert_eq!(text, "read @\"Cargo.toml\"");
        assert_eq!(caret, 18);
    }

    #[test]
    fn nothing_to_offer_means_no_menu() {
        assert!(Completion::at("read @zzz", 9, &Fixed(Vec::new())).is_none());
        assert!(Completion::at("plain words", 6, &source(&["a"])).is_none());
    }

    #[test]
    fn a_subsequence_match_is_in_order() {
        assert!(subsequence_match("wig", "skill:wig"));
        assert!(subsequence_match("", "anything"));
        assert!(!subsequence_match("gw", "wig"), "order matters");
        assert!(!subsequence_match("wigg", "wig"));
    }
}
