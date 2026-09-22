//! The turn loop: ask, stream, run what the model asked for, ask again.
//!
//! One entry point, [`Agent::run`], which answers the last message in a history and leaves that
//! history longer than it found it. Everything a user watches happens as events on the bus, so the
//! loop has no idea whether a terminal is attached.
//!
//! Four decisions, each of which is a bug the other way round:
//!
//! - **The model is a trait.** [`Model`] is one method — stream a turn — and `jmds_api::Client`
//!   implements it. That is the seam a test drives with a script instead of a network, which is
//!   what makes the interesting part of this file (the message bookkeeping between rounds)
//!   testable at all.
//! - **A tool call is only announced once it is whole.** The API streams a call in fragments —
//!   id and name first, arguments accumulating after — so the loop reassembles by index and
//!   publishes [`AgentEvent::ToolCall`] after the stream ends. A transcript that showed fragments
//!   would show arguments that were never the argument.
//! - **Tools run in the order the model asked for them.** Sequential, not parallel: two calls in
//!   one turn are usually a read that the next call depends on, and a pane per call only makes
//!   sense if what a call *did* lands in the transcript in the order it happened. (`tools::queue`
//!   is what would make running them at once safe later, and is not used for that yet.)
//! - **A turn that asks for tools does not end.** The loop appends the assistant's tool calls, runs
//!   them, appends one `tool` message per result, and asks again — up to
//!   [`AgentConfig::max_rounds`], because a model that keeps asking for the same failing call must
//!   not be able to run forever. Hitting the cap is reported as an error event, not silence.

use std::{collections::BTreeMap, future::Future};

use jmds_api::{
    ApiError, ChatMessage, Client, FinishReason as ApiFinish, StreamEvent, ToolCall,
    ToolCallFunction, ToolSpec, enforce_reasoning_replay,
};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::{
    event::{AgentEvent, EventBus, FinishReason, TurnUsage},
    tools::set::ToolOutcome,
};

/// How many ask-and-run cycles one turn may take before the loop stops on its own.
///
/// High enough for real work — a dozen file reads and edits in one turn is ordinary — and finite,
/// because the failure it bounds is a model looping on a call that keeps failing.
pub const DEFAULT_MAX_ROUNDS: usize = 25;

/// What the loop needs from a model: one streamed turn.
///
/// A trait rather than the concrete client so the loop can be tested against a script. It is
/// deliberately the shape of `jmds_api::Client::stream` and nothing more.
pub trait Model {
    fn stream<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
        sink: &'a UnboundedSender<StreamEvent>,
    ) -> impl Future<Output = Result<(), ApiError>> + Send + 'a;
}

impl Model for Client {
    async fn stream<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
        sink: &'a UnboundedSender<StreamEvent>,
    ) -> Result<(), ApiError> {
        Client::stream(self, messages, tools, sink).await
    }
}

/// What the loop needs from the tools: their table, and a way to run one.
pub trait Tools {
    fn specs(&self) -> Vec<ToolSpec>;
    fn call(&self, name: &str, arguments: &str) -> impl Future<Output = ToolOutcome> + Send;
}

impl Tools for crate::tools::set::ToolSet {
    async fn call(&self, name: &str, arguments: &str) -> ToolOutcome {
        crate::tools::set::ToolSet::call(self, name, arguments).await
    }

    fn specs(&self) -> Vec<ToolSpec> {
        crate::tools::set::ToolSet::specs(self)
    }
}

/// How the loop is set up.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The model name sent to the API, and the name the reasoning-replay rule is looked up under.
    pub model: String,
    /// The system prompt, which is the first message of every request.
    pub system: String,
    pub max_rounds: usize,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>, system: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }
}

/// The loop.
pub struct Agent<M, T> {
    model: M,
    tools: T,
    bus: EventBus,
    config: AgentConfig,
}

impl<M: Model, T: Tools> Agent<M, T> {
    pub fn new(model: M, tools: T, bus: EventBus, config: AgentConfig) -> Self {
        Self {
            model,
            tools,
            bus,
            config,
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Answer the last message in `messages`, running the tools the model asks for along the way.
    ///
    /// `messages` is the session's history and is appended to: a user message, then per round an
    /// assistant message and one `tool` message per call. The system message is put in front if it
    /// is not already there.
    pub async fn run(&self, messages: &mut Vec<ChatMessage>) -> Result<(), ApiError> {
        if messages.first().map(|message| message.role) != Some(jmds_api::Role::System) {
            messages.insert(0, ChatMessage::system(self.config.system.clone()));
        }

        self.bus.publish(AgentEvent::TurnStarted {
            model: self.config.model.clone(),
        });

        let specs = self.tools.specs();
        for round in 1..=self.config.max_rounds {
            // DeepSeek's own rule, applied to the request rather than remembered by every caller:
            // an assistant turn from this model carries `reasoning_content`, even when it is empty.
            enforce_reasoning_replay(messages, &self.config.model);

            let turn = self.stream_round(messages, &specs).await?;

            // The partial answer is kept even when the stream failed: a turn that said three
            // sentences before the socket died said three sentences.
            messages.push(turn.assistant_message());
            if let Some(error) = turn.error {
                self.bus.publish(AgentEvent::Error(error));
                return Ok(());
            }

            let finish = turn.finish.clone().unwrap_or(ApiFinish::Stop);
            let calls = turn.calls();
            if calls.is_empty() {
                self.bus.publish(AgentEvent::TurnFinished {
                    reason: reason_into_engine(finish),
                });
                return Ok(());
            }

            // Whole calls only: announced after the stream ended, because until then their
            // arguments were still arriving.
            for call in &calls {
                self.bus.publish(AgentEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                });
            }

            for call in &calls {
                let name = call.function.name.clone();
                let outcome = self.tools.call(&name, &call.function.arguments).await;
                self.bus.publish(AgentEvent::ToolResult {
                    id: call.id.clone(),
                    ok: outcome.ok,
                    summary: outcome.summary,
                });
                messages.push(ChatMessage::tool(call.id.clone(), outcome.content));
            }

            if round == self.config.max_rounds {
                let message = format!(
                    "stopped after {} rounds of tool calls without an answer",
                    self.config.max_rounds
                );
                self.bus.publish(AgentEvent::Error(message.clone()));
                self.bus.publish(AgentEvent::TurnFinished {
                    reason: FinishReason::Other("max_rounds".to_string()),
                });
                return Ok(());
            }
        }
        Ok(())
    }

    /// One request, streamed into a [`Turn`] while its events go onto the bus.
    async fn stream_round(
        &self,
        messages: &[ChatMessage],
        specs: &[ToolSpec],
    ) -> Result<Turn, ApiError> {
        // The stream and its consumer run together: the client writes into the sink as bytes
        // arrive, and this side reads them as they are written. Awaiting the request first would
        // deadlock on a bounded channel and would defeat the point of streaming anyway.
        let (sink, mut events) = mpsc::unbounded_channel();
        // The sink is dropped by the request, not by this function: a channel's receiver only ends
        // when every sender is gone, so a sink that outlived the stream would leave the reader
        // waiting for a turn that had already finished.
        let request = async {
            let result = self.model.stream(messages, specs, &sink).await;
            drop(sink);
            result
        };
        let (result, turn) = tokio::join!(request, async {
            let mut turn = Turn::default();
            while let Some(event) = events.recv().await {
                turn.absorb(event, &self.bus);
            }
            turn
        });
        result?;
        Ok(turn)
    }
}

/// What one streamed turn said, before it is turned into messages and events.
#[derive(Default)]
struct Turn {
    content: String,
    reasoning: String,
    /// Tool call fragments by the index the API gave them, so out-of-order pieces still reassemble
    /// into the call the model meant.
    calls: BTreeMap<usize, PartialCall>,
    finish: Option<ApiFinish>,
    error: Option<String>,
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl Turn {
    /// Take one event: keep what it carries, and tell the bus about the parts that are already
    /// final (`content`, `reasoning`, accounting), leaving the tool calls for the end.
    fn absorb(&mut self, event: StreamEvent, bus: &EventBus) {
        match event {
            StreamEvent::Content(delta) => {
                bus.publish(AgentEvent::Content(delta.clone()));
                self.content.push_str(&delta);
            }
            StreamEvent::Reasoning(delta) => {
                bus.publish(AgentEvent::Thinking(delta.clone()));
                self.reasoning.push_str(&delta);
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let partial = self.calls.entry(index).or_default();
                // Only a non-empty piece counts. The id and the name arrive in the first fragment
                // and later fragments repeat them as empty, so taking the last one seen would erase
                // the tool's name and send the model a call with no callee.
                if let Some(id) = id.filter(|id| !id.is_empty()) {
                    partial.id = Some(id);
                }
                if let Some(name) = name.filter(|name| !name.is_empty()) {
                    partial.name = Some(name);
                }
                if let Some(arguments) = arguments {
                    partial.arguments.push_str(&arguments);
                }
            }
            StreamEvent::Usage(usage) => bus.publish(AgentEvent::Usage(usage_into_engine(usage))),
            StreamEvent::Finished(reason) => self.finish = Some(reason),
            StreamEvent::Error(message) => self.error = Some(message),
        }
    }

    /// The assistant message this turn becomes.
    fn assistant_message(&self) -> ChatMessage {
        let calls = self.calls();
        let mut message = if calls.is_empty() {
            ChatMessage::assistant(self.content.clone())
        } else {
            ChatMessage::assistant_with_tools(self.content.clone(), calls)
        };
        if !self.reasoning.is_empty() {
            message.reasoning_content = Some(self.reasoning.clone());
        }
        message
    }

    fn calls(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .map(|(index, partial)| {
                let id = partial
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{index}"));
                ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: partial.name.clone().unwrap_or_default(),
                        arguments: partial.arguments.clone(),
                    },
                }
            })
            .collect()
    }
}

/// What a turn cost, in the engine's vocabulary.
///
/// Field by field rather than by re-exporting the API's struct: the two layers are allowed to
/// change shape independently, and a cost that stops being reported should be a compile error here
/// rather than a zero on a status line.
fn usage_into_engine(usage: jmds_api::Usage) -> TurnUsage {
    TurnUsage {
        prompt_tokens: usage.input_tokens,
        cache_hit_tokens: usage.prompt_cache_hit_tokens,
        cache_miss_tokens: usage.prompt_cache_miss_tokens,
        completion_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

/// The API's reason for stopping, in the engine's vocabulary.
fn reason_into_engine(reason: ApiFinish) -> FinishReason {
    match reason {
        ApiFinish::Stop => FinishReason::Stop,
        ApiFinish::ToolCalls => FinishReason::ToolCalls,
        ApiFinish::Length => FinishReason::Length,
        ApiFinish::ContentFilter => FinishReason::Other("content_filter".to_string()),
        ApiFinish::Other(other) => FinishReason::Other(other),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use jmds_api::{Role, ToolSpec, Usage};
    use parking_lot::Mutex;
    use tokio::sync::broadcast::Receiver;

    use super::*;
    use crate::event::Event;

    /// The agent events a subscriber has seen so far.
    fn seen(rx: &mut Receiver<Event>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::Agent(agent) = event {
                events.push(agent);
            }
        }
        events
    }

    /// A model that answers from a script and remembers what it was asked.
    #[derive(Default)]
    struct Scripted {
        responses: Mutex<VecDeque<Vec<StreamEvent>>>,
        requests: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl Scripted {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<Vec<ChatMessage>> {
            self.requests.lock().clone()
        }
    }

    impl Model for Scripted {
        async fn stream<'a>(
            &'a self,
            messages: &'a [ChatMessage],
            _tools: &'a [ToolSpec],
            sink: &'a UnboundedSender<StreamEvent>,
        ) -> Result<(), ApiError> {
            self.requests.lock().push(messages.to_vec());
            let script = self.responses.lock().pop_front().unwrap_or_default();
            for event in script {
                let _ = sink.send(event);
            }
            Ok(())
        }
    }

    /// Tools that record what they were asked and answer the same way every time.
    struct Fake {
        calls: Mutex<Vec<(String, String)>>,
        outcome: ToolOutcome,
    }

    impl Fake {
        fn new(outcome: ToolOutcome) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                outcome,
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().clone()
        }
    }

    impl Tools for Fake {
        async fn call(&self, name: &str, arguments: &str) -> ToolOutcome {
            self.calls
                .lock()
                .push((name.to_string(), arguments.to_string()));
            self.outcome.clone()
        }

        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read".into(),
                description: "read something".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]
        }
    }

    fn call_delta(index: usize, id: &str, name: &str, arguments: &str) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index,
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            arguments: Some(arguments.to_string()),
        }
    }

    fn agent(model: Scripted, tools: Fake) -> (Agent<Scripted, Fake>, EventBus) {
        agent_with_rounds(model, tools, DEFAULT_MAX_ROUNDS)
    }

    fn agent_with_rounds(
        model: Scripted,
        tools: Fake,
        rounds: usize,
    ) -> (Agent<Scripted, Fake>, EventBus) {
        let bus = EventBus::new(64);
        let config = AgentConfig::new("deepseek-chat", "be brief").with_max_rounds(rounds);
        let agent = Agent::new(model, tools, bus.clone(), config);
        (agent, bus)
    }

    fn user(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(text)]
    }

    #[tokio::test]
    async fn a_plain_answer_becomes_one_assistant_message_and_its_events() {
        let model = Scripted::new(vec![vec![
            StreamEvent::Content("The answer ".into()),
            StreamEvent::Content("is 4.".into()),
            StreamEvent::Usage(Usage {
                output_tokens: 9,
                reasoning_tokens: 7,
                ..Usage::default()
            }),
            StreamEvent::Finished(ApiFinish::Stop),
        ]]);
        let (agent, bus) = agent(model, Fake::new(ToolOutcome::done("", "")));
        let mut messages = user("what is 2+2?");

        // Subscribe before the turn so nothing is missed.
        let mut rx = bus.subscribe();
        agent.run(&mut messages).await.unwrap();

        assert_eq!(messages.len(), 3, "system, user, assistant");
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].content.as_deref(), Some("be brief"));
        assert_eq!(messages[2].content.as_deref(), Some("The answer is 4."));
        assert!(messages[2].tool_calls.is_empty());

        let events = seen(&mut rx);
        assert_eq!(
            events.first(),
            Some(&AgentEvent::TurnStarted {
                model: "deepseek-chat".into()
            })
        );
        assert!(events.contains(&AgentEvent::Content("The answer ".into())));
        assert!(events.contains(&AgentEvent::Usage(TurnUsage {
            completion_tokens: 9,
            reasoning_tokens: 7,
            ..TurnUsage::default()
        })));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::TurnFinished {
                reason: FinishReason::Stop
            })
        );
    }

    #[tokio::test]
    async fn a_tool_call_is_announced_whole_and_its_result_reaches_the_next_request() {
        // The arguments arrive in two pieces, the way the API sends them.
        let model = Scripted::new(vec![
            vec![
                call_delta(0, "call_1", "read", "{\"path\":"),
                call_delta(0, "", "", "\"a.txt\"}"),
                StreamEvent::Finished(ApiFinish::ToolCalls),
            ],
            vec![
                StreamEvent::Content("a.txt says hello".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
        ]);
        let tools = Fake::new(ToolOutcome::done("read a.txt · 1 line", "hello\n"));
        let (agent, bus) = agent(model, tools);
        let mut messages = user("what is in a.txt?");

        let mut rx = bus.subscribe();
        agent.run(&mut messages).await.unwrap();

        // The call reached the tool assembled, not in fragments.
        assert_eq!(
            agent.tools.calls(),
            vec![("read".to_string(), "{\"path\":\"a.txt\"}".to_string())]
        );

        // And the transcript shows the call once, with its arguments whole.
        let events = seen(&mut rx);
        assert!(events.contains(&AgentEvent::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{\"path\":\"a.txt\"}".into(),
        }));
        assert!(events.contains(&AgentEvent::ToolResult {
            id: "call_1".into(),
            ok: true,
            summary: "read a.txt · 1 line".into(),
        }));

        // The history the second request was built from: system, user, the assistant's call, the
        // tool's answer, and the assistant's reply — which is the same list the caller holds.
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[2].role, Role::Assistant);
        assert_eq!(messages[2].tool_calls.len(), 1);
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].content.as_deref(), Some("hello\n"));
        assert_eq!(messages[4].role, Role::Assistant);
        assert_eq!(agent.model.requests().len(), 2, "the model was asked twice");
    }

    #[tokio::test]
    async fn a_later_empty_fragment_does_not_erase_the_tool_the_call_is_for() {
        // What the wire actually does: the name is in the first fragment and later fragments carry
        // it as an empty string. Taking the last one would call a tool with no name.
        let model = Scripted::new(vec![
            vec![
                call_delta(0, "call_1", "read", "{\"path\":"),
                call_delta(0, "", "", "\"a.txt\"}"),
                StreamEvent::Finished(ApiFinish::ToolCalls),
            ],
            vec![
                StreamEvent::Content("done".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
        ]);
        let tools = Fake::new(ToolOutcome::done("ok", "x"));
        let (agent, _bus) = agent(model, tools);
        let mut messages = user("read it");

        agent.run(&mut messages).await.unwrap();
        assert_eq!(
            agent.tools.calls(),
            vec![("read".to_string(), "{\"path\":\"a.txt\"}".to_string())],
            "the call reached the tool with its name and its whole arguments"
        );
    }

    #[tokio::test]
    async fn a_request_after_a_tool_call_carries_the_reasoning_replay_the_model_requires() {
        // DeepSeek rejects an assistant turn from this model that does not carry
        // `reasoning_content`. The loop has to apply that to the request it builds, whether or not
        // this particular turn thought anything.
        let model = Scripted::new(vec![
            vec![
                StreamEvent::Content("looking".into()),
                call_delta(0, "call_1", "read", "{}"),
                StreamEvent::Finished(ApiFinish::ToolCalls),
            ],
            vec![
                StreamEvent::Content("done".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
        ]);
        let (agent, _bus) = agent(model, Fake::new(ToolOutcome::done("ok", "x")));
        let mut messages = user("go");

        agent.run(&mut messages).await.unwrap();

        let requests = agent.model.requests();
        let second = &requests[1];
        let assistant = second
            .iter()
            .find(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
            .expect("the assistant's tool call is in the second request");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some(""),
            "an empty reasoning field, which is what the API wants when the model did not think"
        );
    }

    #[tokio::test]
    async fn a_reasoning_turn_keeps_its_thinking_in_the_message_and_in_the_transcript() {
        let model = Scripted::new(vec![vec![
            StreamEvent::Reasoning("let me ".into()),
            StreamEvent::Reasoning("think".into()),
            StreamEvent::Content("4".into()),
            StreamEvent::Finished(ApiFinish::Stop),
        ]]);
        let (agent, bus) = agent(model, Fake::new(ToolOutcome::done("", "")));
        let mut messages = user("2+2?");

        let mut rx = bus.subscribe();
        agent.run(&mut messages).await.unwrap();

        assert!(messages[2].has_reasoning());
        assert_eq!(
            messages[2].reasoning_content.as_deref(),
            Some("let me think")
        );
        let events = seen(&mut rx);
        assert!(events.contains(&AgentEvent::Thinking("let me ".into())));
        assert!(events.contains(&AgentEvent::Content("4".into())));
    }

    #[tokio::test]
    async fn a_tool_that_failed_still_answers_the_model_and_the_loop_carries_on() {
        let model = Scripted::new(vec![
            vec![
                call_delta(0, "call_1", "read", "{\"path\":\"missing\"}"),
                StreamEvent::Finished(ApiFinish::ToolCalls),
            ],
            vec![
                StreamEvent::Content("it is not there".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
        ]);
        let tools = Fake::new(ToolOutcome::failed(
            "read failed: no such file",
            "no such file",
        ));
        let (agent, bus) = agent(model, tools);
        let mut messages = user("read missing");

        let mut rx = bus.subscribe();
        agent.run(&mut messages).await.unwrap();

        let events = seen(&mut rx);
        assert!(events.contains(&AgentEvent::ToolResult {
            id: "call_1".into(),
            ok: false,
            summary: "read failed: no such file".into(),
        }));
        // system, user, the assistant's call, the tool's answer, the assistant's reply.
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[4].content.as_deref(), Some("it is not there"));
    }

    #[tokio::test]
    async fn a_model_that_keeps_asking_for_tools_is_stopped_and_told() {
        // Every round asks again: without the cap this never ends.
        let round = || {
            vec![
                call_delta(0, "call_x", "read", "{}"),
                StreamEvent::Finished(ApiFinish::ToolCalls),
            ]
        };
        let model = Scripted::new(vec![round(), round(), round(), round()]);
        let (agent, bus) = agent_with_rounds(model, Fake::new(ToolOutcome::done("ok", "x")), 2);
        let mut messages = user("loop forever");
        let bus = bus;
        let mut rx = bus.subscribe();

        agent.run(&mut messages).await.unwrap();

        assert_eq!(agent.tools.calls().len(), 2, "two rounds, then the cap");
        let events = seen(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(message) if message.contains("stopped after 2 rounds")
        )));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::TurnFinished {
                reason: FinishReason::Other("max_rounds".into())
            })
        );
    }

    #[tokio::test]
    async fn a_stream_that_failed_keeps_what_it_said_and_reports_the_failure() {
        let model = Scripted::new(vec![vec![
            StreamEvent::Content("half an ans".into()),
            StreamEvent::Error("upstream closed the connection".into()),
        ]]);
        let (agent, bus) = agent(model, Fake::new(ToolOutcome::done("", "")));
        let mut messages = user("say something");

        let mut rx = bus.subscribe();
        agent.run(&mut messages).await.unwrap();

        assert_eq!(messages[2].content.as_deref(), Some("half an ans"));
        let events = seen(&mut rx);
        assert!(events.contains(&AgentEvent::Error("upstream closed the connection".into())));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnFinished { .. })),
            "a turn that failed did not finish"
        );
    }

    #[tokio::test]
    async fn the_loop_can_be_run_twice_over_the_same_history() {
        // The second question must not lose the first turn's messages, and the system prompt must
        // not be added a second time.
        let model = Scripted::new(vec![
            vec![
                StreamEvent::Content("first".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
            vec![
                StreamEvent::Content("second".into()),
                StreamEvent::Finished(ApiFinish::Stop),
            ],
        ]);
        let (agent, _bus) = agent(model, Fake::new(ToolOutcome::done("", "")));
        let mut messages = user("one");
        agent.run(&mut messages).await.unwrap();

        messages.push(ChatMessage::user("two"));
        agent.run(&mut messages).await.unwrap();

        let requests = agent.model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]
                .iter()
                .filter(|m| m.role == Role::System)
                .count(),
            1
        );
        assert!(
            requests[1]
                .iter()
                .any(|m| m.content.as_deref() == Some("first"))
        );
        assert_eq!(
            messages.len(),
            5,
            "system, user, assistant, user, assistant"
        );
    }
}
