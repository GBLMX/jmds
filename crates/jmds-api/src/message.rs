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

/// What goes in place of reasoning that was never recorded.
///
/// A constant rather than an empty string: the API rejects an empty `reasoning_content`, and a
/// reader of a session file can tell "this was never recorded" from "the model thought nothing".
pub const REASONING_REPLAY_PLACEHOLDER: &str = "(reasoning not recorded)";

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

/// Give every tool-calling assistant turn a `reasoning_content`, in place.
///
/// Returns how many turns were patched — zero for a model that does not require it, and zero for a
/// conversation that already carries its reasoning. Patching is deliberately the narrow fix:
/// nothing is reordered, nothing is dropped, so the prefix of the conversation is untouched and the
/// prefix cache survives.
pub fn enforce_reasoning_replay(messages: &mut [ChatMessage], model: &str) -> usize {
    if !requires_reasoning_replay(model) {
        return 0;
    }
    let mut patched = 0;
    for message in messages {
        if message.asks_for_tools() && !message.has_reasoning() {
            message.reasoning_content = Some(REASONING_REPLAY_PLACEHOLDER.into());
            patched += 1;
        }
    }
    patched
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
    fn reasoning_is_replayed_only_where_it_is_owed() {
        let mut messages = vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant_with_tools("", vec![ToolCall::new("c1", "read", "{}")]),
            ChatMessage::tool("c1", "ok"),
            ChatMessage::assistant("done"),
        ];
        assert_eq!(enforce_reasoning_replay(&mut messages, "deepseek-chat"), 1);

        assert_eq!(
            messages[1].reasoning_content.as_deref(),
            Some(REASONING_REPLAY_PLACEHOLDER)
        );
        // A plain answer owes nothing: there is no tool call it has to be consistent with.
        assert!(messages[3].reasoning_content.is_none());
        assert!(messages[0].reasoning_content.is_none());
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
            Some(REASONING_REPLAY_PLACEHOLDER)
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
