//! The response side: SSE framing, and the deltas inside a chunk.
//!
//! Two jobs, kept apart on purpose. [`SseBuffer`] knows nothing about DeepSeek — it turns a byte
//! stream into the payloads of complete `data:` events, which is a framing problem. [`parse_delta`]
//! knows nothing about framing — it reads one JSON payload into the events it carries, which is a
//! schema problem. A chunk boundary can fall anywhere (inside a JSON string, inside a multi-byte
//! character), so only the buffer is allowed to care about where the last read ended.

use serde::{Deserialize, Serialize};

use crate::usage::Usage;

/// Why the model stopped.
///
/// The wire's own vocabulary, kept separate from the engine's: this crate talks to the API, and
/// `jmds-core` maps this into the event the app reacts to. `ToolCalls` is the one that means "do
/// not stop — run them and ask again".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    /// A reason a newer API version introduced. Kept as text so a new one cannot crash a session.
    #[serde(untagged)]
    Other(String),
}

/// Something the model said, thought, or asked for while a response streamed in.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Visible answer text. Deltas: a subscriber appends.
    Content(String),
    /// The model's reasoning, in the same delta shape.
    Reasoning(String),
    /// A tool call, or a fragment of one.
    ///
    /// The API streams a call in pieces: the id and the name arrive in the first fragment and the
    /// arguments accumulate across the rest, all sharing an `index`. Reassembly belongs to the
    /// caller, which is why the index is part of the event.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    /// Token accounting. DeepSeek sends it in a final chunk of its own when the request asked for
    /// it, which is why a chunk with no choices can still carry something.
    Usage(Usage),
    Finished(FinishReason),
    /// An error the API reported *inside* the stream, after a 200.
    ///
    /// Not the same thing as a failed request: the client turns a non-200 into its own error, and
    /// this is the case that would otherwise be a stream that simply ends.
    Error(String),
}

/// Turns a byte stream into the payloads of complete `data:` events.
#[derive(Debug, Default)]
pub struct SseBuffer {
    /// Everything since the last newline. A chunk can end anywhere, so the tail waits here.
    pending: String,
    /// The `data:` lines of the event being assembled. Server-sent events put several of them in
    /// one event and join them with newlines; DeepSeek sends one, and the join costs nothing.
    data: Vec<String>,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk and take out whatever events it completed.
    ///
    /// Fields other than `data` (`event:`, `id:`, `retry:`) and comment lines (a leading `:`,
    /// which is how a server keeps a connection alive) are read and dropped: this API does not use
    /// them, and a keep-alive must not be mistaken for content.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();

        // Only whole lines are consumed; an unterminated tail stays in `pending` for the next
        // chunk, which is what makes a split inside a JSON string harmless.
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending[..newline].trim_end_matches('\r').to_string();
            self.pending.drain(..=newline);

            if line.is_empty() {
                // A blank line ends the event.
                if !self.data.is_empty() {
                    out.push(self.data.join("\n"));
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                self.data
                    .push(payload.strip_prefix(' ').unwrap_or(payload).to_string());
            }
        }

        out
    }
}

/// Read one SSE payload into the events it carries.
///
/// A payload that is not JSON, or that holds nothing this app acts on (a role-only first chunk, a
/// keep-alive shaped like `{}`), yields an empty vector rather than an error: the stream is the
/// API's, and an unrecognised field is not a reason to fail a turn.
pub fn parse_delta(payload: &str) -> Result<Vec<StreamEvent>, serde_json::Error> {
    // The end-of-stream sentinel is not JSON.
    if payload.trim() == "[DONE]" {
        return Ok(vec![StreamEvent::Finished(FinishReason::Stop)]);
    }

    let value: serde_json::Value = serde_json::from_str(payload)?;
    let mut out = Vec::new();

    if let Some(message) = value.get("error").and_then(describe_error) {
        out.push(StreamEvent::Error(message));
    }

    // A chunk that carries usage carries nothing else — it is the accounting that arrives after
    // the last choice.
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        out.push(StreamEvent::Usage(Usage::from_json(usage)));
    }

    if let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) {
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(|text| text.as_str())
                && !text.is_empty()
            {
                out.push(StreamEvent::Content(text.to_string()));
            }
            if let Some(text) = delta
                .get("reasoning_content")
                .and_then(|text| text.as_str())
                && !text.is_empty()
            {
                out.push(StreamEvent::Reasoning(text.to_string()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|calls| calls.as_array()) {
                for (position, call) in calls.iter().enumerate() {
                    out.push(StreamEvent::ToolCallDelta {
                        // The API sends the index; a server that omits it means "in order".
                        index: call
                            .get("index")
                            .and_then(|index| index.as_u64())
                            .map_or(position, |index| index as usize),
                        id: call
                            .get("id")
                            .and_then(|id| id.as_str())
                            .map(str::to_string),
                        name: call
                            .pointer("/function/name")
                            .and_then(|name| name.as_str())
                            .map(str::to_string),
                        arguments: call
                            .pointer("/function/arguments")
                            .and_then(|arguments| arguments.as_str())
                            .map(str::to_string),
                    });
                }
            }
        }

        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(|reason| reason.as_str())
        {
            out.push(StreamEvent::Finished(match reason {
                "stop" => FinishReason::Stop,
                "tool_calls" => FinishReason::ToolCalls,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                other => FinishReason::Other(other.to_string()),
            }));
        }
    }

    Ok(out)
}

/// Pull a readable message out of an error object, whatever shape it came in.
fn describe_error(error: &serde_json::Value) -> Option<String> {
    if let Some(text) = error.as_str() {
        return Some(text.to_string());
    }
    let message = error
        .get("message")
        .and_then(|message| message.as_str())
        .unwrap_or("the API reported an error");
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(|code| code.as_str());
    Some(match code {
        Some(code) => format!("{message} ({code})"),
        None => message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Content(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_event_split_across_chunks_is_read_once_it_is_whole() {
        let mut buffer = SseBuffer::new();
        assert!(buffer.push("data: {\"choi").is_empty());
        assert!(buffer.push("ces\":[{\"delta\":").is_empty());
        let payloads = buffer.push("{\"content\":\"hi\"}}]}\n\n");
        assert_eq!(payloads.len(), 1);
        let events = parse_delta(&payloads[0]).unwrap();
        assert_eq!(content(&events), "hi");
    }

    #[test]
    fn two_events_in_one_chunk_come_out_as_two() {
        let mut buffer = SseBuffer::new();
        let payloads = buffer.push("data: a\n\ndata: b\n\n");
        assert_eq!(payloads, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn carriage_returns_and_keep_alives_do_not_become_content() {
        let mut buffer = SseBuffer::new();
        let payloads = buffer.push(": keep-alive\r\n\r\ndata: {\"x\":1}\r\n\r\n");
        assert_eq!(payloads, vec!["{\"x\":1}".to_string()]);
    }

    #[test]
    fn a_field_this_api_does_not_use_is_ignored() {
        let mut buffer = SseBuffer::new();
        let payloads = buffer.push("event: message\ndata: {\"x\":1}\n\n");
        assert_eq!(payloads, vec!["{\"x\":1}".to_string()]);
    }

    #[test]
    fn the_done_sentinel_ends_the_stream_rather_than_parsing_as_json() {
        let events = parse_delta("[DONE]").unwrap();
        assert_eq!(events, vec![StreamEvent::Finished(FinishReason::Stop)]);
    }

    #[test]
    fn a_first_chunk_that_only_names_the_role_carries_nothing() {
        let events = parse_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn answer_text_and_reasoning_arrive_as_separate_events() {
        let payload = r#"{"choices":[{"delta":{"reasoning_content":"hmm","content":"answer"}}]}"#;
        let events = parse_delta(payload).unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::Content("answer".into()),
                StreamEvent::Reasoning("hmm".into()),
            ]
        );
    }

    #[test]
    fn an_empty_delta_is_not_an_empty_string_of_content() {
        // Providers send `content: ""` for a reasoning-only chunk; appending it would be harmless
        // but it would also mean every such chunk looked like a piece of the answer.
        let events = parse_delta(r#"{"choices":[{"delta":{"content":""}}]}"#).unwrap();
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn tool_call_fragments_keep_their_index_and_accumulate() {
        let first = parse_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(
            first,
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("read".into()),
                arguments: Some("{\"pa".into()),
            }]
        );

        let second = parse_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(
            second,
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("th\":\"a\"}".into()),
            }]
        );
    }

    #[test]
    fn the_final_chunk_reports_usage_and_the_reason_it_stopped() {
        let payload = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],
                          "usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":80,"completion_tokens":7}}"#;
        let events = parse_delta(payload).unwrap();
        assert!(events.contains(&StreamEvent::Finished(FinishReason::ToolCalls)));
        assert!(events.contains(&StreamEvent::Usage(Usage {
            input_tokens: 100,
            prompt_cache_hit_tokens: 80,
            prompt_cache_miss_tokens: 20,
            output_tokens: 7,
            reasoning_tokens: 0,
        })));
    }

    #[test]
    fn a_finish_reason_a_newer_api_added_is_kept_as_text() {
        let events =
            parse_delta(r#"{"choices":[{"delta":{},"finish_reason":"something_new"}]}"#).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Finished(FinishReason::Other(
                "something_new".into()
            ))]
        );
    }

    #[test]
    fn an_error_inside_the_stream_is_reported_with_its_code() {
        let events = parse_delta(
            r#"{"error":{"message":"context length exceeded","code":"invalid_request"}}"#,
        )
        .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Error(
                "context length exceeded (invalid_request)".into()
            )]
        );
    }

    #[test]
    fn an_unparsable_payload_is_an_error_and_not_a_panic() {
        assert!(parse_delta("{not json").is_err());
        // …but a payload that parses and holds nothing this app reads is simply empty.
        assert!(
            parse_delta(r#"{"object":"chat.completion.chunk"}"#)
                .unwrap()
                .is_empty()
        );
    }
}
