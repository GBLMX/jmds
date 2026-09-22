//! jmds: the binary.
//!
//! The one place the three crates meet, and the only place that touches the process: the terminal's
//! modes, the runtime, the clock that animates the spinner, the session file, and the task that
//! owns the conversation.
//!
//! Three decisions worth naming:
//!
//! - **The conversation has one owner.** The turn task holds the message history and appends to it,
//!   and a turn is one at a time. Sharing the history behind a lock would let a second question
//!   start while the first answer is still arriving, and the two would interleave into a transcript
//!   that matches neither.
//! - **Nothing is drawn until something is ready.** The loop uses `select!` over terminal input,
//!   the bus and an 80 ms tick, and redraws once per wakeup. The terminal's own diffing is what
//!   makes that cheap; the tick is what animates a pane that has something to show mid-turn.
//! - **The terminal is put back on every path out**, including a panic — `ratatui::init` installs
//!   the hook that does it — and the modes this app adds on top (bracketed paste, the kitty
//!   keyboard flag) are taken back off before the alternate screen goes away, because the two
//!   screens keep separate stacks for them.

use std::{io, path::PathBuf, time::Duration};

use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind};
use futures_util::StreamExt;
use jmds_api::{ChatMessage, Client, ClientConfig};
use jmds_core::{
    agent::{Agent, AgentConfig},
    config::Config,
    event::{AgentEvent, Event as BusEvent, EventBus},
    paths::sessions_dir,
    prompt::{Prompt, default_prompt_path},
    session::{Header, SessionFile, new_id},
    tools::set::ToolSet,
};
use jmds_tui::{
    app::{Action, App},
    pane::{chat::Chat, editor::Editor, terminal::TerminalPane},
    terminal::{
        COLOR_MODE, ColorMode, begin_synchronized_update, disable_terminal_modes,
        enable_terminal_modes, end_synchronized_update,
    },
    theme::{GlyphSet, Theme},
};
use tokio::{sync::mpsc, task::JoinHandle};

/// How often a frame may be drawn while nothing is happening. Slow enough to be free, fast enough
/// that a spinner looks like it is moving.
const TICK: Duration = Duration::from_millis(80);

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

async fn run() -> color_eyre::Result<()> {
    let config = Config::load();
    if let Err(error) = jmds_core::logger::init_logger(&config) {
        // A logger that cannot start is not a reason to refuse to run: the app's output is the
        // terminal, and the log is for afterwards.
        eprintln!("jmds: 日志没能启动（{error}）—— 继续，只是没有日志文件");
    }

    let cwd = std::env::current_dir()?;
    let bus = EventBus::new(256);
    let theme = choose_theme(&config);

    // Raw mode, the alternate screen, and the panic hook that puts them back.
    let mut terminal = ratatui::init();
    let mut out = io::stdout();
    // On top of that: bracketed paste, and the kitty keyboard flag that makes `Esc` unambiguous.
    let modes = enable_terminal_modes(&mut out);

    let outcome = session_loop(&mut terminal, &config, theme, cwd, bus).await;

    let _ = disable_terminal_modes(&mut out);
    ratatui::restore();
    if let Err(error) = modes {
        log::warn!("终端模式没能打开: {error}");
    }
    outcome
}

/// The event loop: input, the bus, and the clock.
async fn session_loop(
    terminal: &mut ratatui::DefaultTerminal,
    config: &Config,
    theme: Theme,
    cwd: PathBuf,
    bus: EventBus,
) -> color_eyre::Result<()> {
    let (prompts, questions) = mpsc::unbounded_channel::<String>();
    // Built here rather than in the task: the task outlives this borrow, so what it needs is
    // handed over by value.
    let client = config.api.api_key().map(|key| {
        Client::new(ClientConfig::new(
            config.api.base_url.clone(),
            config.api.model.clone(),
            key,
        ))
    });
    let conversation = spawn_conversation(
        bus.clone(),
        client,
        config.api.model.clone(),
        config.api.api_key_env.clone(),
        system_prompt(&cwd),
        sessions_dir(),
        cwd.clone(),
        questions,
    );

    let mut app = App::new();
    app.host_mut().set_theme(theme);

    // The conversation takes the whole area first, then the prompt file splits it. Focus stays in
    // the chat because that is where a turn is started from; the editor is where the *next*
    // question gets written.
    let chat = app.open(jmds_core::pane::Axis::Horizontal, Chat::new());
    match open_prompt_pane(&cwd, config.editor.tab_width) {
        // Opened while it has focus, so the next split divides *it* rather than the chat.
        Some(editor) => {
            app.open(jmds_core::pane::Axis::Horizontal, editor);
        }
        None => log::warn!("提示词文件打不开，这次只有对话面板"),
    }
    match open_shell_pane(&cwd) {
        Some(shell) => {
            app.open(jmds_core::pane::Axis::Vertical, shell);
        }
        None => log::warn!("shell 起不来，这次没有终端面板"),
    }
    // Focus ends in the chat: that is where a turn is started from.
    app.host_mut().focus(chat);

    let mut out = io::stdout();
    let mut input = EventStream::new();
    let mut events = bus.subscribe();
    let mut ticker = tokio::time::interval(TICK);

    loop {
        // One frame, written as one update: without this a redraw is visible while it is being
        // written, which is exactly what makes an 80 ms animation flicker.
        begin_synchronized_update(&mut out)?;
        let drawn = terminal.draw(|frame| app.draw(frame.area(), frame.buffer_mut()));
        end_synchronized_update(&mut out)?;
        drawn?;

        tokio::select! {
            incoming = input.next() => match incoming {
                Some(Ok(TermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                    if app.on_key(key) == Action::Quit {
                        break;
                    }
                    // What the human pressed Enter on, on its way to the one owner of the history.
                    // What the human pressed Enter on. The app's own commands run here and are
                    // answered here; only prose goes on to the one owner of the history, so a
                    // mistyped command is never answered by the model.
                    let mut quit = false;
                    for line in app.take_requests() {
                        match app.handle_command(&line) {
                            Some(jmds_tui::app::CommandOutcome::Quit) => {
                                quit = true;
                                break;
                            }
                            Some(jmds_tui::app::CommandOutcome::Handled) => {}
                            None => {
                                let _ = prompts.send(line);
                            }
                        }
                    }
                    if quit {
                        break;
                    }
                }
                Some(Ok(TermEvent::Resize(..))) => {}
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    log::warn!("读终端输入失败: {error}");
                    break;
                }
                None => break,
            },
            incoming = events.recv() => match incoming {
                Ok(BusEvent::Agent(agent)) => {
                    app.on_agent_event(&agent);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    // Being told is the point: a pane that fell behind shows a gap rather than a
                    // transcript with a silent hole in it.
                    log::warn!("落下了 {missed} 条事件");
                }
                Err(_) => break,
            },
            _ = ticker.tick() => app.tick(),
        }
    }

    // The sender goes away with the loop, and the conversation task ends when it does.
    drop(prompts);
    let _ = conversation.await;
    Ok(())
}

/// The task that owns the conversation: one turn at a time, in one history, written to one file.
///
/// Everything it needs is passed by value. It outlives the loop that starts it, so it cannot borrow
/// the configuration the loop was reading.
#[allow(clippy::too_many_arguments)]
fn spawn_conversation(
    bus: EventBus,
    client: Option<Result<Client, jmds_api::ApiError>>,
    model: String,
    key_env: String,
    system: String,
    sessions: PathBuf,
    cwd: PathBuf,
    mut questions: mpsc::UnboundedReceiver<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut store =
            match SessionFile::create(&sessions, Header::new(new_id(), model.clone(), cwd.clone()))
                .await
            {
                Ok(store) => Some(store),
                Err(error) => {
                    // A session that cannot be written is worth saying out loud, but not worth
                    // refusing to talk.
                    bus.publish(AgentEvent::Error(format!(
                        "会话文件写不了（{error}），这一轮不会留下记录"
                    )));
                    None
                }
            };

        let client = match client {
            Some(Ok(client)) => Some(client),
            Some(Err(error)) => {
                bus.publish(AgentEvent::Error(format!("客户端建不起来: {error}")));
                None
            }
            None => {
                log::warn!("没有 API key：把 {key_env} 设好之后才有回答");
                None
            }
        };

        let Some(client) = client else {
            // Without a client there is still a conversation to answer: say why, in the transcript,
            // for every question asked. `TurnStarted` first, so the pane stops showing a spinner
            // when the error closes the turn.
            while let Some(_question) = questions.recv().await {
                bus.publish(AgentEvent::TurnStarted {
                    model: model.clone(),
                });
                bus.publish(AgentEvent::Error(format!(
                    "没有可用的 DeepSeek 客户端：把 key 放进 {key_env} 再启动"
                )));
            }
            return;
        };

        let agent = Agent::new(
            client,
            ToolSet::new(&cwd),
            bus.clone(),
            AgentConfig::new(model.clone(), system),
        );

        while let Some(question) = questions.recv().await {
            // Everything from here on is what this turn added, which is exactly what goes to the
            // file: the human's question, then whatever the loop appended after it. The system
            // message is added by the loop on the first turn, above the mark, so it is written too.
            let mark = messages.len();
            messages.push(ChatMessage::user(question));
            if let Err(error) = agent.run(&mut messages).await {
                bus.publish(AgentEvent::Error(error.to_string()));
            }
            if let Some(store) = store.as_mut()
                && let Err(error) = store.append_messages(&messages[mark..]).await
            {
                bus.publish(AgentEvent::Error(format!("会话没写下去: {error}")));
            }
        }
    })
}

/// The prompt-file pane.
///
/// The file is written from the template the first time, which is the one file this app creates
/// without being asked: a format nobody can see is a format nobody uses, and the alternative is an
/// editor pane that opens onto nothing with no hint of what belongs in it.
fn open_prompt_pane(cwd: &std::path::Path, tab_width: u8) -> Option<Editor> {
    let path = default_prompt_path();
    if !path.exists() {
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("建不了 {}: {error}", parent.display());
            return None;
        }
        if let Err(error) = std::fs::write(&path, Prompt::template()) {
            log::warn!("写不了 {}: {error}", path.display());
        }
    }
    match Editor::open(&path, cwd) {
        Ok(editor) => Some(editor.with_tab_width(tab_width.max(1) as usize)),
        Err(error) => {
            log::warn!("{} 打不开: {error}", path.display());
            None
        }
    }
}

/// A shell, in its own pane.
///
/// The user's own shell (`$SHELL`), in the session's directory: the pane is for the commands a
/// person runs by hand, which is why it is a real PTY and not the `bash` tool.
fn open_shell_pane(cwd: &std::path::Path) -> Option<TerminalPane> {
    let shell = TerminalPane::shell();
    // A placeholder size: the first draw tells the PTY what the pane actually got.
    match TerminalPane::spawn(&shell, &[], cwd, (24, 80)) {
        Ok(pane) => Some(pane),
        Err(error) => {
            log::warn!("{shell} 起不来: {error}");
            None
        }
    }
}

/// What the model is told about itself and where it is.
///
/// Short on purpose: what each tool is *for* is in the tool table, which the model is sent anyway,
/// and repeating it here is how the two drift apart.
fn system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are jmds, a coding assistant working in a terminal.\n\
         The session's working directory is {}; relative paths in tool calls resolve against it.\n\
         Use the tools to look at the file system instead of guessing about it, and keep answers \
         short: the human is reading them in a terminal.",
        cwd.display()
    )
}

/// The theme the config asks for, in what this terminal can actually show.
fn choose_theme(config: &Config) -> Theme {
    let mode = match config.theme.color_mode.as_str() {
        "truecolor" => ColorMode::TrueColor,
        "ansi256" => ColorMode::Ansi256,
        "ansi" => ColorMode::Basic,
        // `auto`, or anything else a config file might say: the probe's answer.
        _ => *COLOR_MODE,
    };
    let glyphs = match config.theme.glyphs.as_str() {
        "ascii" => GlyphSet::Ascii,
        _ => GlyphSet::Unicode,
    };
    // An unknown name is a typo in a colour scheme, not a reason to refuse to start.
    Theme::named(&config.theme.name)
        .unwrap_or_default()
        .with_glyphs(glyphs)
        .downsampled(mode)
}
