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

use std::{io, path::Path, path::PathBuf, time::Duration};

use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind};
use futures_util::StreamExt;
use jmds_api::{ChatMessage, Client, ClientConfig};
use jmds_core::{
    agent::{Agent, AgentConfig},
    config::Config,
    event::{AgentEvent, Event as BusEvent, EventBus, SessionEvent},
    pane::{Axis, PaneId},
    paths::sessions_dir,
    prompt::{Prompt, default_prompt_path},
    pty::Run,
    session::{Header, SessionFile, branch, by_id, latest_in, new_id, recover},
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

mod cli;

/// How often a frame may be drawn while nothing is happening. Slow enough to be free, fast enough
/// that a spinner looks like it is moving.
const TICK: Duration = Duration::from_millis(80);

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // Parsed before anything else: a command line that cannot be understood should say so on a
    // terminal that still looks like a terminal.
    let arguments: Vec<String> = std::env::args().collect();
    let args = match cli::parse(&arguments[1..]) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("jmds: {message}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if args.start == cli::Start::Help {
        println!("{}", cli::USAGE);
        return Ok(());
    }
    if args.start == cli::Start::Version {
        println!("jmds {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

async fn run(args: cli::Args) -> color_eyre::Result<()> {
    let config = Config::load();
    if let Err(error) = jmds_core::logger::init_logger(&config) {
        // A logger that cannot start is not a reason to refuse to run: the app's output is the
        // terminal, and the log is for afterwards.
        eprintln!("jmds: 日志没能启动（{error}）—— 继续，只是没有日志文件");
    }

    let cwd = std::env::current_dir()?;
    let bus = EventBus::new(256);
    // Resolved before the terminal is taken: a session that cannot be opened is not something to
    // discover through a full-screen app, and a `--resume` that fell back to a new conversation
    // would look exactly like a resume that lost its history.
    let resolved = match resolve(
        &args.start,
        args.keep,
        &cwd,
        &sessions_dir(),
        &config.api.model,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(message) => {
            eprintln!("jmds: {message}");
            std::process::exit(2);
        }
    };
    let theme = choose_theme(&config);

    // Raw mode, the alternate screen, and the panic hook that puts them back.
    let mut terminal = ratatui::init();
    let mut out = io::stdout();
    // On top of that: bracketed paste, and the kitty keyboard flag that makes `Esc` unambiguous.
    let modes = enable_terminal_modes(&mut out);

    let outcome = session_loop(&mut terminal, &config, theme, cwd, bus, resolved).await;

    let _ = disable_terminal_modes(&mut out);
    ratatui::restore();
    if let Err(error) = modes {
        log::warn!("终端模式没能打开: {error}");
    }
    outcome
}

/// Where a session starts: a new file, or one already on disk.
#[derive(Debug)]
enum Opening {
    /// A brand new conversation, in a file of its own.
    New,
    /// Continuing something: the file to keep appending to, what it already said, and — for a
    /// branch — the session it was branched off.
    Continued {
        path: PathBuf,
        messages: Vec<ChatMessage>,
        branched_from: Option<String>,
    },
}

/// A command-line request resolved against what is on disk.
#[derive(Debug)]
struct Resolved {
    opening: Opening,
    /// Which model this conversation talks to. A continued session keeps its own: that is the model
    /// its history came from, and swapping it mid-conversation changes what the history means.
    model: String,
}

/// Turn a request into a session file and a history to start from.
///
/// Everything that can go wrong here is the caller's to say out loud: the answer is a message for
/// the terminal, not a panic and not a silent fallback to a new conversation.
async fn resolve(
    request: &cli::Start,
    keep: Option<usize>,
    cwd: &Path,
    sessions: &Path,
    model: &str,
) -> Result<Resolved, String> {
    let summary = match request {
        cli::Start::New | cli::Start::Help | cli::Start::Version => {
            return Ok(Resolved {
                opening: Opening::New,
                model: model.to_string(),
            });
        }
        cli::Start::Continue => disk(latest_in(sessions, cwd), "读会话目录")?
            .ok_or_else(|| format!("这个目录里还没有会话可以接着聊：{}", cwd.display()))?,
        cli::Start::Resume(id) => {
            disk(by_id(sessions, id), "读会话")?.ok_or_else(|| format!("找不到会话 {id}"))?
        }
        cli::Start::Branch { from, .. } => match from {
            Some(id) => {
                disk(by_id(sessions, id), "读会话")?.ok_or_else(|| format!("找不到会话 {id}"))?
            }
            None => disk(latest_in(sessions, cwd), "读会话目录")?
                .ok_or_else(|| format!("这个目录里还没有会话可以分叉：{}", cwd.display()))?,
        },
    };

    // The history has to be about *this* directory. Handing the model a conversation about files it
    // cannot see is worse than starting over, because it will answer about them anyway.
    if summary.header.cwd != cwd {
        return Err(format!(
            "会话 {} 是在 {} 里进行的，不是 {}——换个目录进来，或者开一个新的",
            summary.id(),
            summary.header.cwd.display(),
            cwd.display()
        ));
    }

    let (path, branched_from) = match request {
        cli::Start::Branch { .. } => {
            // Everything by default: a branch that quietly dropped most of the history would be a
            // conversation with amnesia, which is the opposite of why anyone branches.
            let id = new_id();
            let path = disk(
                branch(&summary.path, keep.unwrap_or(usize::MAX), &id).await,
                "分叉会话",
            )?;
            (path, Some(summary.header.id.clone()))
        }
        _ => (summary.path.clone(), None),
    };

    let recovered = disk(recover(&path).await, "读会话内容")?;
    if !recovered.is_complete() {
        // A session whose tail never got written — a crash mid-turn. What is there is still worth
        // continuing from, and saying so beats pretending the history is whole.
        log::warn!(
            "会话 {} 末尾有一行看不下去，它后面的内容已忽略",
            path.display()
        );
    }

    Ok(Resolved {
        opening: Opening::Continued {
            path,
            messages: recovered.messages(),
            branched_from,
        },
        model: summary.header.model.clone(),
    })
}

/// An `io::Result` with somewhere for the failure to go: the command line's answer, not a panic.
fn disk<T>(result: std::io::Result<T>, what: &str) -> Result<T, String> {
    result.map_err(|error| format!("{what}失败：{error}"))
}

/// The event loop: input, the bus, and the clock.
async fn session_loop(
    terminal: &mut ratatui::DefaultTerminal,
    config: &Config,
    theme: Theme,
    cwd: PathBuf,
    bus: EventBus,
    resolved: Resolved,
) -> color_eyre::Result<()> {
    let (prompts, work) = mpsc::unbounded_channel::<Work>();
    // Built here rather than in the task: the task outlives this borrow, so what it needs is
    // handed over by value. The model is the session's, which for a continued conversation is the
    // one its history came from rather than whatever the config says today.
    let model = resolved.model.clone();
    let client = config.api.api_key().map(|key| {
        Client::new(ClientConfig::new(
            config.api.base_url.clone(),
            model.clone(),
            key,
        ))
    });
    // Subscribed before the conversation task starts: the task announces the session the moment it
    // has opened it — and hands over a continued session's history — and a subscriber that arrived
    // afterwards would see neither, leaving a resumed conversation looking brand new.
    let mut events = bus.subscribe();
    let conversation = spawn_conversation(
        bus.clone(),
        client,
        model,
        config.api.api_key_env.clone(),
        system_prompt(&cwd),
        sessions_dir(),
        cwd.clone(),
        resolved.opening,
        work,
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
    // The shell: a pane first, then a command in it. The pane is named before the command exists
    // because the events about that command are routed by that name — and the process belongs to the
    // engine rather than to the pane, because closing a split and stopping a program are different
    // decisions that were being made by the same object.
    let shell = shell_program();
    let shell_id = PaneId::fresh();
    // Kept alive for the length of the session: dropping the run stops the shell.
    let _shell_run = match Run::spawn(shell_id, &shell, "", (24, 80), bus.clone()) {
        Ok(run) => {
            app.host_mut().open_as(
                shell_id,
                Axis::Vertical,
                TerminalPane::new(shell_id, shell_title(&shell), (24, 80)),
            );
            Some(run)
        }
        Err(error) => {
            log::warn!("{shell} 起不来，这次没有终端面板：{error}");
            None
        }
    };
    // The file tree, beside the shell: what changed, next to the thing that changes it. It opens
    // after the shell so the split divides the lower half rather than the whole right column.
    app.open(
        jmds_core::pane::Axis::Horizontal,
        jmds_tui::pane::files::FileTree::new(cwd.clone()),
    );
    // Focus ends in the chat: that is where a turn is started from.
    app.host_mut().focus(chat);

    let mut out = io::stdout();
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);

    // Watching the project, so whoever shows files hears about changes instead of polling a
    // directory every frame. Alive for the length of the session: dropping it stops the watch. A
    // watch that cannot start is worth a warning, not the end of the app — everything else works.
    let _watcher = match jmds_core::watch::Watcher::watch(&cwd, bus.clone()) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            log::warn!("文件监视起不来，文件面板不会自动更新：{error}");
            None
        }
    };

    loop {
        // One frame, and only when there is something to put in it: the tick is a clock, and most
        // ticks have nothing to show. Drawing anyway re-lays-out the whole transcript twelve times a
        // second to write the same pixels back, a cost that grows with the conversation.
        if app.wants_frame() {
            // Written as one update: without this a redraw is visible while it is being written,
            // which is exactly what makes an 80 ms animation flicker.
            begin_synchronized_update(&mut out)?;
            let drawn = terminal.draw(|frame| app.draw(frame.area(), frame.buffer_mut()));
            end_synchronized_update(&mut out)?;
            drawn?;
        }

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
                            Some(jmds_tui::app::CommandOutcome::Resume(id)) => {
                                let _ = prompts.send(Work::Resume(id));
                            }
                            Some(jmds_tui::app::CommandOutcome::Handled) => {}
                            None => {
                                let _ = prompts.send(Work::Ask(line));
                            }
                        }
                    }
                    if quit {
                        break;
                    }
                    // What the panes want said to the processes they show: keys already encoded as
                    // a terminal would send them, and the size the pane was given. They go on the
                    // bus because the pane has no process to write to and the app has no keyboard
                    // encoding — each half does what it knows.
                    for event in app.take_pty() {
                        bus.publish(event);
                    }
                }
                Some(Ok(TermEvent::Mouse(mouse))) => {
                    app.on_mouse(mouse);
                }
                Some(Ok(TermEvent::Resize(..))) => app.touch(),
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
                Ok(BusEvent::File(file)) => {
                    app.on_file_event(&file);
                }
                Ok(BusEvent::Pty(pty)) => {
                    app.on_pty_event(&pty);
                }
                Ok(BusEvent::Pane(pane)) => {
                    app.on_pane_event(&pane);
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
/// What the loop asks the conversation task to do.
///
/// One channel rather than two, because these are the same kind of thing: something only the one owner
/// of the history can act on, in the order it arrived.
enum Work {
    /// Ask the model something.
    Ask(String),
    /// Continue a different session, held in this directory.
    Resume(String),
}

fn spawn_conversation(
    bus: EventBus,
    client: Option<Result<Client, jmds_api::ApiError>>,
    model: String,
    key_env: String,
    system: String,
    sessions: PathBuf,
    cwd: PathBuf,
    opening: Opening,
    mut work: mpsc::UnboundedReceiver<Work>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Where this conversation's history lives, and whether it started here. A session that
        // cannot be written is worth saying out loud, but not worth refusing to talk.
        let (mut messages, mut store) = match opening {
            Opening::New => {
                let id = new_id();
                let header = Header::new(id.clone(), model.clone(), cwd.clone());
                match SessionFile::create(&sessions, header).await {
                    Ok(store) => {
                        bus.publish(SessionEvent::Started { id });
                        (Vec::new(), Some(store))
                    }
                    Err(error) => {
                        bus.publish(AgentEvent::Error(format!(
                            "会话文件写不了（{error}），这一轮不会留下记录"
                        )));
                        (Vec::new(), None)
                    }
                }
            }
            Opening::Continued {
                path,
                messages,
                branched_from,
            } => match SessionFile::open(&path).await {
                Ok(store) => {
                    let id = store.header().id.clone();
                    match branched_from {
                        Some(from) => bus.publish(SessionEvent::Branched { from, to: id }),
                        None => bus.publish(SessionEvent::Restored { id }),
                    }
                    // The history the model is about to be given, handed to whoever is watching:
                    // otherwise a continued conversation looks brand new to the person reading it
                    // while the model answers about things that were said in it.
                    bus.publish(AgentEvent::History(messages.clone()));
                    (messages, Some(store))
                }
                Err(error) => {
                    // The history is still worth continuing with even if it cannot be added to.
                    bus.publish(AgentEvent::Error(format!(
                        "会话文件打不开（{error}），这一轮不会留下记录"
                    )));
                    (messages, None)
                }
            },
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
            while let Some(work) = work.recv().await {
                // Switching sessions needs no client: it is a file and a history, not a request. The key
                // decides whether questions can be *answered*, and someone without one may well want to
                // read what was said last time.
                if let Work::Resume(id) = work {
                    resume_into(&id, &sessions, &cwd, &mut messages, &mut store, &bus).await;
                    continue;
                }
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
            ToolSet::new(&cwd).with_bus(bus.clone()),
            bus.clone(),
            AgentConfig::new(model.clone(), system),
        );

        while let Some(work) = work.recv().await {
            let question = match work {
                Work::Ask(question) => question,
                // A switch happens between turns by construction: this loop runs them one at a time,
                // so nothing is in flight while the history is being replaced.
                Work::Resume(id) => {
                    resume_into(&id, &sessions, &cwd, &mut messages, &mut store, &bus).await;
                    continue;
                }
            };
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

/// The user's own shell, in its own pane: `$SHELL`, or `sh` when the environment does not say.
///
/// This pane is for the commands a person runs by hand, which is why it is a real pty and not the
/// `bash` tool.
fn shell_program() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "sh".to_string())
}

/// A shell's name, for the pane's title: the last segment of its path.
fn shell_title(shell: &str) -> String {
    shell
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(shell)
        .to_string()
}

/// Continue another session, saying why if it cannot be done.
///
/// Used by both loops — the one with a client and the one without — because switching between
/// conversations is a file operation: it must not depend on whether the model can be reached.
async fn resume_into(
    id: &str,
    sessions: &Path,
    cwd: &Path,
    messages: &mut Vec<ChatMessage>,
    store: &mut Option<SessionFile>,
    bus: &EventBus,
) {
    if let Err(why) = switch_session(id, sessions, cwd, messages, store, bus).await {
        bus.publish(AgentEvent::Error(why));
    }
}

/// Move this conversation to another session: read it, and start appending there.
///
/// The rules are the ones starting in a session follows, for the same reasons: a session held in
/// another directory is refused, because the model would be handed a history about files it cannot
/// see; and a file that cannot be read leaves the current conversation exactly as it was, rather than
/// half-switched.
async fn switch_session(
    id: &str,
    sessions: &Path,
    cwd: &Path,
    messages: &mut Vec<ChatMessage>,
    store: &mut Option<SessionFile>,
    bus: &EventBus,
) -> Result<(), String> {
    let summary = by_id(sessions, id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("找不到会话 {id}"))?;
    if summary.header.cwd != cwd {
        return Err(format!(
            "会话 {} 是在 {} 里进行的，不是 {}",
            summary.id(),
            summary.header.cwd.display(),
            cwd.display()
        ));
    }
    let recovered = recover(&summary.path)
        .await
        .map_err(|error| format!("读会话内容失败：{error}"))?;
    let opened = SessionFile::open(&summary.path)
        .await
        .map_err(|error| format!("会话文件打不开（{error}）"))?;

    // The store is replaced, not kept: the old file stops being written to the moment this succeeds,
    // which is what "continue that one instead" means.
    *messages = recovered.messages();
    *store = Some(opened);
    bus.publish(SessionEvent::Restored {
        id: summary.header.id.clone(),
    });
    // The pane is told to start over with what was read: appending to the transcript already on screen
    // would leave two conversations in one scroll.
    bus.publish(AgentEvent::History(messages.clone()));
    Ok(())
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

#[cfg(test)]
mod tests {
    use jmds_core::{
        event::{AgentEvent, Event, EventBus},
        pane::Axis,
        watch::Watcher,
    };
    use jmds_tui::{app::App, pane::files::FileTree};

    use super::{Opening, Work, cli, resolve, spawn_conversation, switch_session};
    use crate::PaneId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use jmds_core::pty::Run;

    /// What the loop in [`session_loop`] does with one bus event, without a terminal.
    fn drawn(app: &mut App) -> String {
        let area = ratatui::layout::Rect::new(0, 0, 60, 14);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        app.draw(area, &mut buffer);
        let mut out = String::new();
        for row in 0..area.height {
            for column in 0..area.width {
                out.push_str(buffer[(column, row)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// The whole chain the app depends on: a watcher on the session's directory, the bus, the app's
    /// fan-out, and the file tree. Every link has its own tests; this one exists because the chain
    /// is the part that can be wired wrong while all the links pass.
    #[tokio::test]
    async fn a_file_written_by_someone_else_turns_up_in_the_file_tree() {
        let dir = std::env::temp_dir().join(format!("jmds-wiring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bus = EventBus::new(64);
        let _watcher = Watcher::watch(&dir, bus.clone()).expect("一个监视器");
        let mut app = App::new();
        app.open(Axis::Horizontal, FileTree::new(dir.clone()));
        let mut events = bus.subscribe();

        std::fs::write(dir.join("delta.txt"), "written from outside\n").unwrap();

        // Wait for whatever the watcher publishes, then hand it to the app exactly as the loop does.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut handed = 0;
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(Event::File(file)) => {
                    app.on_file_event(&file);
                    handed += 1;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(error) => panic!("总线不该断开：{error:?}"),
            }
            if handed > 0 && drawn(&mut app).contains("delta.txt") {
                break;
            }
        }

        let screen = drawn(&mut app);
        assert!(handed > 0, "监视器没有发布任何文件事件：\n{screen}");
        assert!(
            screen.contains("delta.txt"),
            "事件到了，但文件树没显示它：\n{screen}"
        );
    }

    /// The last link of the pty chain: a key pressed in a terminal pane, through the app's outbox,
    /// the bus, and the engine, into the command the pane is showing.
    ///
    /// The pane's own tests prove it encodes a key; the engine's prove an `Input` event reaches a
    /// process. Neither can see the app in between, and the app is where a pane's outbox is
    /// published at all.
    #[tokio::test]
    async fn a_key_pressed_in_a_terminal_pane_reaches_the_command_it_shows() {
        let id = PaneId::fresh();
        let bus = EventBus::new(64);
        let _run = Run::spawn(id, "sh", "cat", (10, 40), bus.clone()).expect("一个 pty");

        let mut app = App::new();
        app.host_mut().open_as(
            id,
            Axis::Horizontal,
            jmds_tui::pane::terminal::TerminalPane::new(id, "cat", (10, 40)),
        );
        // What the loop does with a key, and then with what the panes want said.
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let events = app.take_pty();
        assert!(
            matches!(
                events.as_slice(),
                [jmds_core::event::PtyEvent::Input { .. }, ..]
            ),
            "面板该把按键交出来：{events:?}"
        );
        for event in events {
            bus.publish(event);
        }

        // `cat` echoes what it reads, so the letter coming back on the pane's own screen is the
        // whole round trip: key -> app -> bus -> pty -> process -> output -> pane.
        let mut incoming = bus.subscribe();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if drawn(&mut app).contains('q') {
                return;
            }
            // The pane only learns about output through the loop, so the loop's other half has to
            // be here too: take what the engine published and hand it to the app.
            while let Ok(event) = incoming.try_recv() {
                if let Event::Pty(pty) = event {
                    app.on_pty_event(&pty);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("按键没有走到进程：\n{}", drawn(&mut app));
    }

    /// The whole point of the tool pane, end to end: a `bash` call asked for by the agent turns into
    /// a pane showing its output, and a `Ctrl+C` typed into that pane stops the call.
    ///
    /// Three links are tested where they live — the engine opens panes and stops on `0x03`, the app
    /// publishes what its panes ask for, the pane renders bytes — and this one exists because the
    /// chain is what can be wired wrong while every link passes.
    #[tokio::test]
    async fn a_bash_call_shows_in_a_pane_and_a_ctrl_c_there_stops_it() {
        use jmds_core::event::PaneEvent;
        use jmds_core::tools::set::ToolSet;

        let bus = EventBus::new(256);
        let mut app = App::new();
        let mut incoming = bus.subscribe();
        let tools = ToolSet::new("/tmp").with_bus(bus.clone());

        // The call runs the way the agent runs it: the tool set, with the bus, called by name.
        let call = tokio::spawn(async move {
            tools
                .call("bash", r#"{"command":"echo from-the-tool; sleep 30"}"#)
                .await
        });

        // The loop's two halves, without a terminal: the engine asks for panes, and pty events go to
        // the pane they are about.
        let mut tool_pane = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            while let Ok(event) = incoming.try_recv() {
                match event {
                    Event::Pane(pane) => {
                        if let PaneEvent::Opened { spec } = &pane {
                            tool_pane = Some(spec.id);
                        }
                        app.on_pane_event(&pane);
                    }
                    Event::Pty(pty) => app.on_pty_event(&pty),
                    _ => {}
                }
            }
            if tool_pane.is_some() && drawn(&mut app).contains("from-the-tool") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pane = tool_pane.expect("引擎该为这次调用开一个面板");
        assert!(
            drawn(&mut app).contains("from-the-tool"),
            "工具的输出该在它自己的面板上：\n{}",
            drawn(&mut app)
        );

        // `Ctrl+C` in that pane, the way a person does it: focus it, press the key, and let the app
        // publish what the pane wants said.
        app.host_mut().focus(pane);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        for event in app.take_pty() {
            bus.publish(event);
        }

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("被叫停的调用该很快回来")
            .expect("任务不该 panic");
        assert!(outcome.ok, "{}", outcome.content);
        assert!(
            outcome.summary.contains("interrupted"),
            "结果该说它是被叫停的：{}",
            outcome.summary
        );
    }

    /// A session file for the given directory, with the given messages in it.
    fn session_file(
        sessions: &std::path::Path,
        id: &str,
        cwd: &std::path::Path,
    ) -> std::path::PathBuf {
        let path = sessions.join(format!("{id}.jsonl"));
        let header = format!(
            r#"{{"kind":"header","id":"{id}","model":"deepseek-chat","cwd":"{}","started_at_ms":1758550000000}}"#,
            cwd.display()
        );
        let lines = [
            header,
            r#"{"kind":"message","role":"user","content":"上一次问的问题"}"#.to_string(),
            r#"{"kind":"message","role":"assistant","content":"上一次给的回答"}"#.to_string(),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[tokio::test]
    async fn a_switch_needs_no_api_key() {
        // The bug this pins, found by running the thing: with no key in the environment, switching
        // sessions was refused with "no client" — the guard meant for questions, applied to a file
        // operation. Someone without a key may still want to read what was said last time.
        let dir = std::env::temp_dir().join(format!("jmds-switch-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        session_file(&dir, "700-0", &dir);

        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let (prompts, work) = tokio::sync::mpsc::unbounded_channel();
        let conversation = spawn_conversation(
            bus.clone(),
            None,
            "deepseek-chat".to_string(),
            "DEEPSEEK_API_KEY".to_string(),
            "system".to_string(),
            dir.clone(),
            dir.clone(),
            Opening::New,
            work,
        );
        prompts
            .send(Work::Resume("700-0".to_string()))
            .expect("任务还在");

        let mut switched = false;
        let mut errors: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !switched {
            match events.try_recv() {
                Ok(Event::Agent(AgentEvent::History(_))) => switched = true,
                Ok(Event::Agent(AgentEvent::Error(why))) => errors.push(why),
                Ok(_) => {}
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
        assert!(switched, "没有 key 也该换得过去：{errors:?}");
        assert!(errors.is_empty(), "更不该报错：{errors:?}");
        conversation.abort();
    }

    #[tokio::test]
    async fn switching_sessions_replaces_the_history_and_the_file() {
        let dir = std::env::temp_dir().join(format!("jmds-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = session_file(&dir, "700-0", &dir);

        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let mut messages: Vec<jmds_api::ChatMessage> = Vec::new();
        let mut store = None;

        switch_session("700-0", &dir, &dir, &mut messages, &mut store, &bus)
            .await
            .expect("这个世界里会话是有的");

        assert_eq!(messages.len(), 2, "读回来的是它的历史");
        assert_eq!(
            store.expect("也要接着往那个文件里写").path(),
            target,
            "并且是这一个文件"
        );
        let published: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| format!("{event:?}"))
            .collect();
        assert!(
            published.iter().any(|event| event.contains("Restored")),
            "{published:?}"
        );
        assert!(
            published.iter().any(|event| event.contains("History")),
            "面板也要被告知换了一段对话：{published:?}"
        );
    }

    #[tokio::test]
    async fn switching_to_a_session_from_another_directory_is_refused_and_changes_nothing() {
        let dir =
            std::env::temp_dir().join(format!("jmds-switch-elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        session_file(&dir, "700-0", std::path::Path::new("/somewhere/else"));

        let bus = EventBus::new(64);
        let mut messages: Vec<jmds_api::ChatMessage> =
            vec![jmds_api::ChatMessage::user("还说着一半的话")];
        let mut store = None;

        let error = switch_session("700-0", &dir, &dir, &mut messages, &mut store, &bus)
            .await
            .expect_err("别人的项目不该接上");
        assert!(error.contains("/somewhere/else"), "{error}");
        assert_eq!(messages.len(), 1, "半路失败不该把现在这段也弄丢");
        assert!(store.is_none(), "也没有换到另一个文件上");
    }

    #[tokio::test]
    async fn a_resumed_session_hands_its_history_to_the_panes() {
        let dir = std::env::temp_dir().join(format!("jmds-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        session_file(&sessions, "500-0", &dir);

        let resolved = resolve(
            &cli::Start::Resume("500-0".into()),
            None,
            &dir,
            &sessions,
            "cfg-model",
        )
        .await
        .expect("这个世界里会话是有的");
        // The session's own model, not the configured one.
        assert_eq!(resolved.model, "deepseek-chat");

        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let mut app = App::new();
        app.open(Axis::Horizontal, jmds_tui::pane::chat::Chat::new());
        // What the conversation task publishes for a continued session.
        let Opening::Continued { messages, .. } = &resolved.opening else {
            panic!("该是接着聊，不是新开");
        };
        assert_eq!(messages.len(), 2, "两条留言都读回来了");
        bus.publish(AgentEvent::History(messages.clone()));
        // And what main's loop does with it.
        while let Ok(event) = events.try_recv() {
            if let Event::Agent(agent) = event {
                app.on_agent_event(&agent);
            }
        }

        let screen = drawn(&mut app);
        // Wide characters take two cells, so the pane pads them and the screen has spaces between
        // the characters of a Chinese line. Compared without the padding, which is a rendering
        // detail rather than part of what was said.
        let squeezed = screen.replace(' ', "");
        assert!(squeezed.contains("上一次问的问题"), "{screen}");
        assert!(squeezed.contains("上一次给的回答"), "{screen}");
    }

    #[tokio::test]
    async fn resuming_a_session_from_another_directory_is_refused() {
        let dir = std::env::temp_dir().join(format!("jmds-elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        session_file(&dir, "500-0", std::path::Path::new("/somewhere/else"));

        let error = resolve(&cli::Start::Resume("500-0".into()), None, &dir, &dir, "m")
            .await
            .expect_err("别人的项目不该接上");
        assert!(error.contains("/somewhere/else"), "{error}");
    }

    #[tokio::test]
    async fn branching_keeps_the_history_and_points_at_where_it_came_from() {
        let dir = std::env::temp_dir().join(format!("jmds-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        session_file(&dir, "500-0", &dir);

        let resolved = resolve(
            &cli::Start::Branch {
                from: Some("500-0".into()),
                keep: None,
            },
            None,
            &dir,
            &dir,
            "m",
        )
        .await
        .expect("分得动");
        let Opening::Continued {
            path,
            messages,
            branched_from,
        } = &resolved.opening
        else {
            panic!("分支也是接着聊");
        };
        assert_eq!(branched_from.as_deref(), Some("500-0"));
        assert_eq!(messages.len(), 2, "分叉保留全部历史");
        assert_ne!(path, &dir.join("500-0.jsonl"), "新会话写在新文件里");
        // And the new file says where it came from.
        let written = std::fs::read_to_string(path).unwrap();
        assert!(written.contains(r#""branched_from":"500-0""#), "{written}");
    }

    #[tokio::test]
    async fn branching_can_keep_only_the_first_few_messages() {
        let dir = std::env::temp_dir().join(format!("jmds-branch-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        session_file(&dir, "500-0", &dir);

        let resolved = resolve(
            &cli::Start::Branch {
                from: Some("500-0".into()),
                keep: None,
            },
            Some(1),
            &dir,
            &dir,
            "m",
        )
        .await
        .expect("分得动");
        let Opening::Continued { messages, .. } = &resolved.opening else {
            panic!("分支也是接着聊");
        };
        assert_eq!(messages.len(), 1, "只留前一条");
    }

    #[tokio::test]
    async fn a_new_conversation_needs_nothing_on_disk() {
        let dir = std::env::temp_dir().join(format!("jmds-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let resolved = resolve(&cli::Start::New, None, &dir, &dir, "cfg-model")
            .await
            .expect("新开不需要任何东西");
        assert_eq!(resolved.model, "cfg-model", "新会话用配置里的模型");
        assert!(matches!(resolved.opening, Opening::New));

        // And asking to continue where nothing was said says so, rather than starting fresh.
        let error = resolve(&cli::Start::Continue, None, &dir, &dir, "m")
            .await
            .expect_err("没得接就该说");
        assert!(error.contains(&dir.display().to_string()), "{error}");
    }
}
