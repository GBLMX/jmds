//! The request side: what one message is, and the one obligation DeepSeek adds to it.
//!
//! The format is OpenAI's chat format with an extension — an assistant turn may carry
//! `reasoning_content` beside `content` — and with an obligation attached to that extension: for
//! the models that produce reasoning, an assistant message that carries tool calls has to send
//! that reasoning *back* with it, or the request is rejected.
//!
//! History does not always have it. A session recorded by an older build, a turn whose reasoning
//! the user asked not to keep, a branch that was compacted — all of them can hold an assistant
//! tool call with no reasoning. [`enforce_reasoning_replay`] fills that hole with a placeholder
//! instead of dropping the turn, because dropping it would lose the tool call the answer depends
//! on.

use serde::{Deserialize, Serialize};

/// What is replayed when a turn's reasoning was never recorded.
///
/// An **empty string**, which is what DeepSeek accepts. Not a human-readable placeholder: the
/// reasoning is part of the cached prefix, so anything invented here changes the bytes of every
/// replayed turn, and inventing text the model never produced is not this tool's decision to make.
pub const REASONING_REPLAY_UNKNOWN: &str = "";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One tool call, as the API describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"` from this API; carried so a message round-trips byte for byte.
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// The raw JSON the model produced. Parsing it belongs to the tool, not to the transcript: a
    /// call that is stored unparsed can be replayed even after the tool's schema changed.
    pub arguments: String,
}

fn function_type() -> String {
    "function".into()
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: function_type(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.function.name
    }

    pub fn arguments(&self) -> &str {
        &self.function.arguments
    }
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The model's reasoning, returned with the turn and required back on a turn that carried tool
    /// calls (see [`enforce_reasoning_replay`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on a [`Role::Tool`] message: which call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn bare(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::bare(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::bare(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::bare(Role::Assistant, content)
    }

    /// An assistant turn that asked for tools — the kind that owes reasoning back.
    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::bare(Role::Assistant, content)
        }
    }

    /// What a tool answered.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            tool_call_id: Some(tool_call_id.into()),
            ..Self::bare(Role::Tool, content)
        }
    }

    /// Whether this turn asked for tools, which is what makes the reasoning obligatory.
    pub fn asks_for_tools(&self) -> bool {
        self.role == Role::Assistant && !self.tool_calls.is_empty()
    }

    /// Proxy that thinks; anything else does not.
    pub fn has_reasoning(&self) -> bool {
        self.reasoning_content
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
    }
}

/// Whether this model requires reasoning to be replayed with its tool calls.
///
/// Written as "everything DeepSeek ships except the models that are not reasoning models" rather
/// than as a list of the ones that are: the cost of replaying reasoning the API did not need is a
/// few cached tokens, and the cost of *not* replaying it when it was needed is a rejected request
/// in the middle of a session. The unknown-model default is the safe side of that trade.
pub fn requires_reasoning_replay(model: &str) -> bool {
    // "Anything from this provider" rather than a list of the models that currently need it: a
    // name added tomorrow (`deepseek-v5`, a variant behind a flag) is then already handled, and
    // the cost of replaying reasoning that was not needed is a few cached tokens.
    model.trim().to_ascii_lowercase().starts_with("deepseek")
}

/// Give every assistant turn a `reasoning_content`, in place.
///
/// *Every* turn, not only the ones that asked for tools: DeepSeek's contract is that the reasoning
/// is part of the conversation being replayed, and dropping it is not a legal way to save tokens —
/// the turn that is missing it is the turn whose prefix stops matching.
///
/// Returns how many turns were patched: zero for a model that does not require it, and zero for a
/// conversation that already carries its reasoning. Patching changes nothing else — nothing is
/// reordered, nothing is dropped — so what the provider cached stays a prefix.
pub fn enforce_reasoning_replay(messages: &mut [ChatMessage], model: &str) -> usize {
    if !requires_reasoning_replay(model) {
        return 0;
    }
    let mut patched = 0;
    for message in messages {
        if message.role == Role::Assistant && !message.has_reasoning() {
            message.reasoning_content = Some(REASONING_REPLAY_UNKNOWN.into());
            patched += 1;
        }
    }
    patched
}

/// Remove special template tokens that leaked into visible text.
///
/// The model's own control tokens (`<｜end▁of▁thinking｜>` and friends) belong to the template, not
/// to the answer, and they show up in streamed content often enough that every harness that talks
/// to this API has a stripper. Anything shaped like `<｜…｜>` goes: the alternative is a transcript
/// with template debris in it, and a *different* byte sequence than the one the model produced when
/// the turn is replayed.
pub fn strip_special_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // `｜` (U+FF5C) is what the template uses on both sides of a token's name.
        match after.find('｜') {
            Some(open)
                if after[..open]
                    .chars()
                    .all(|c| matches!(c, '<' | '｜' | '▁' | '|')) =>
            {
                match after[open + '｜'.len_utf8()..].find('｜') {
                    Some(close) => {
                        let end = open + '｜'.len_utf8() + close + '｜'.len_utf8();
                        let tail = &after[end..];
                        if let Some(gt) = tail.find('>') {
                            rest = &tail[gt + 1..];
                            continue;
                        }
                        out.push_str(after);
                        rest = "";
                    }
                    None => {
                        out.push_str(after);
                        rest = "";
                    }
                }
            }
            _ => {
                out.push('<');
                rest = &after['<'.len_utf8()..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_tool_call_round_trips_in_the_shape_the_api_uses() {
        let call = ToolCall::new("call_1", "read", "{\"path\":\"src/main.rs\"}");
        let value = serde_json::to_value(&call).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "call_1",
                "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"src/main.rs\"}" },
            })
        );
        assert_eq!(serde_json::from_value::<ToolCall>(value).unwrap(), call);
    }

    #[test]
    fn empty_fields_are_left_out_of_the_request() {
        // `content: null` and `tool_calls: []` are not what the API expects; a request that omits
        // them is the one the API documents.
        let value = serde_json::to_value(ChatMessage::user("hi")).unwrap();
        assert_eq!(value, json!({ "role": "user", "content": "hi" }));
    }

    #[test]
    fn a_tool_answer_names_the_call_it_answers() {
        let value = serde_json::to_value(ChatMessage::tool("call_1", "3 lines")).unwrap();
        assert_eq!(
            value,
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "3 lines" })
        );
    }

    #[test]
    fn every_assistant_turn_owes_its_reasoning() {
        // Not only the turns that called a tool: DeepSeek's contract is about the conversation
        // being replayed, and a plain answer whose reasoning was dropped is exactly the turn that
        // stops matching the cached prefix.
        let mut messages = vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant_with_tools("", vec![ToolCall::new("c1", "read", "{}")]),
            ChatMessage::tool("c1", "ok"),
            ChatMessage::assistant("done"),
        ];
        assert_eq!(enforce_reasoning_replay(&mut messages, "deepseek-chat"), 2);

        for turn in [&messages[1], &messages[3]] {
            assert_eq!(
                turn.reasoning_content.as_deref(),
                Some(REASONING_REPLAY_UNKNOWN)
            );
        }
        // What is replayed when nothing was recorded is nothing — not a sentence this app made up.
        assert_eq!(
            REASONING_REPLAY_UNKNOWN, "",
            "an invented placeholder would change the prefix"
        );
        // Nobody else's turn carries a reasoning field.
        assert!(messages[0].reasoning_content.is_none());
        assert!(messages[2].reasoning_content.is_none());
    }

    #[test]
    fn special_template_tokens_are_stripped_from_visible_text() {
        assert_eq!(
            strip_special_tokens("answer<｜end▁of▁thinking｜>"),
            "answer",
            "a control token is not part of the answer"
        );
        assert_eq!(strip_special_tokens("a<｜begin▁of▁sentence｜>b"), "ab");
        // Text that merely uses angle brackets is left alone.
        assert_eq!(strip_special_tokens("a < b > c"), "a < b > c");
        assert_eq!(strip_special_tokens("Vec<String>"), "Vec<String>");
        assert_eq!(strip_special_tokens("<｜unterminated"), "<｜unterminated");
    }

    #[test]
    fn reasoning_that_was_recorded_is_left_exactly_as_it_was() {
        let mut turn =
            ChatMessage::assistant_with_tools("", vec![ToolCall::new("c1", "read", "{}")]);
        turn.reasoning_content = Some("first I look, then I leap".into());
        let mut messages = vec![turn];
        assert_eq!(
            enforce_reasoning_replay(&mut messages, "deepseek-reasoner"),
            0
        );
        assert_eq!(
            messages[0].reasoning_content.as_deref(),
            Some("first I look, then I leap")
        );
    }

    #[test]
    fn a_turn_whose_reasoning_is_blank_is_treated_as_unrecorded() {
        // Whitespace is what a session file holds when the reasoning was dropped in a rewrite.
        let mut turn =
            ChatMessage::assistant_with_tools("", vec![ToolCall::new("c1", "read", "{}")]);
        turn.reasoning_content = Some("   \n".into());
        let mut messages = vec![turn];
        assert_eq!(enforce_reasoning_replay(&mut messages, "deepseek-chat"), 1);
        assert_eq!(
            messages[0].reasoning_content.as_deref(),
            Some(REASONING_REPLAY_UNKNOWN)
        );
    }

    #[test]
    fn nothing_is_patched_for_a_model_that_does_not_ask_for_it() {
        let mut messages = vec![ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall::new("c1", "read", "{}")],
        )];
        assert_eq!(
            enforce_reasoning_replay(&mut messages, "some-other-model"),
            0
        );
        assert!(messages[0].reasoning_content.is_none());
    }

    #[test]
    fn the_replay_rule_covers_deepseek_and_defaults_to_the_safe_side() {
        for model in [
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek-v4",
            "DeepSeek-R1",
        ] {
            assert!(requires_reasoning_replay(model), "{model}");
        }
        // An unknown name is not assumed to be a model that tolerates missing reasoning.
        assert!(requires_reasoning_replay("deepseek-something-new"));
        assert!(!requires_reasoning_replay("gpt-4o"));
    }
}
