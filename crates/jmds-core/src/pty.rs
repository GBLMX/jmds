//! 引擎拥有的 pty 会话：进程在这里，面板只在别处画它。
//!
//! 一次会话是一个 [`Run`]：它按 `PaneId` 起一个命令，把主端的输出作为 [`PtyEvent::Output`] 发到
//! 总线上，并把总线上给同一个 id 的 [`PtyEvent::Input`]、[`PtyEvent::Resize`] 和 [`PtyEvent::Kill`]
//! 收回进程里。面板因此不碰文件描述符，也不碰进程组；它只知道这一格该多大、用户按下了什么。
//!
//! 三件事值得说明：
//!
//! - **读输出和收输入各一条线程。** 读主端的那次 `read` 是阻塞的，而一个闲着的 shell 什么也不
//!   输出 —— 输入要是排在那条线程里，用户按下的键就得等到命令下一次打印才进得了 pty，而对交互式
//!   shell 来说那一刻永远不来。
//! - **子进程自己一组。** portable-pty 起进程时 `setsid()` 过，所以子进程是会话首进程、组号就是
//!   它的 pid；本模块杀的是 `-pid` 这一组，`sleep` 也好、`cargo test` 也好，都随这一格一起走。
//! - **结束是事件，不是错误。** 子进程一死，主端读到的就是 `EIO`。那不是异常，是这一会话的最后
//!   一句话，所以它变成一次 [`PtyEvent::Exited`]，然后读线程收工。

use std::{
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
#[cfg(not(unix))]
use portable_pty::ChildKiller;
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};
use tokio::sync::broadcast;

use crate::{
    event::{Event, EventBus, PtyEvent},
    pane::PaneId,
};

/// 一次从主端读多少。pty 没有行结构，读到多少就发多少，所以不必攒着。
const READ_CHUNK: usize = 8192;

/// 引擎拥有的一个 pty 会话。
pub struct Run {
    id: PaneId,
    /// `Run` 自己、读线程和输入线程都只通过它说话。
    shared: Arc<Shared>,
}

/// 那几条线程共用的东西。
///
/// 放在 `Mutex` 里的是 `!Sync` 的句柄（主端、写端）：输入线程必须能自己动手写，它没法反过来叫醒
/// `Run` —— `Run` 那边没有循环在读它。
struct Shared {
    /// 会话首进程的 pid，同时就是进程组号。
    #[cfg(unix)]
    pid: u32,
    /// 还活着吗。读线程发 `Exited` 之前把它置 false；`kill` 立刻置 false。
    running: AtomicBool,
    /// 只用来 `resize`：尺寸由面板决定，而面板的尺寸随时可能变。
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// 写端只能取一次，所以 `Run::write` 和输入线程共用这一个。
    writer: Mutex<Box<dyn Write + Send>>,
    /// Windows 没有进程组，退回 portable-pty 的杀进程。
    #[cfg(not(unix))]
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Shared {
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.lock().write_all(bytes)
    }

    fn resize(&self, size: (u16, u16)) -> io::Result<()> {
        self.master.lock().resize(pty_size(size)).map_err(io_error)
    }

    /// 杀整个进程组。
    fn kill(&self) {
        self.running.store(false, Ordering::Relaxed);
        #[cfg(unix)]
        {
            // 子进程是 `setsid()` 过的会话首进程，组号等于它的 pid，所以负 pid 打的就是这一组。
            // SIGKILL 而不是 SIGTERM：面板已经关了，没人再等它优雅收尾，而一个忽略 SIGTERM 的
            // 子进程会把孤儿留给下一次登录。
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.killer.lock().kill();
        }
    }
}

impl Run {
    /// 在 `id` 名下起一个命令。`shell` 是可执行文件（例如 `/bin/zsh`），`command` 是交给它的
    /// 一行（空串表示交互式 shell）。
    pub fn spawn(
        id: PaneId,
        shell: &str,
        command: &str,
        size: (u16, u16),
        bus: EventBus,
    ) -> io::Result<Run> {
        let PtyPair { master, slave } = native_pty_system()
            .openpty(pty_size(size))
            .map_err(io_error)?;

        let mut builder = CommandBuilder::new(shell);
        if !command.is_empty() {
            builder.arg("-c");
            builder.arg(command);
        }
        // 没有 TERM 的程序会以为自己在往管道里写：颜色、光标定位、行编辑全关掉，而面板要画的
        // 是一个终端。
        builder.env("TERM", "xterm-256color");
        let mut child = slave.spawn_command(builder).map_err(io_error)?;
        // 本进程这一侧的从端要丢掉：只要它还开着，主端就读不到 EOF，`Exited` 也就永远不来。
        drop(slave);

        let reader = master.try_clone_reader().map_err(io_error)?;
        let writer = master.take_writer().map_err(io_error)?;
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            master: Mutex::new(master),
            writer: Mutex::new(writer),
            // 按组杀要有 pid，而 Unix 上 portable-pty 一定给得出。
            #[cfg(unix)]
            pid: session_pid(&mut child)?,
            #[cfg(not(unix))]
            killer: Mutex::new(child.clone_killer()),
        });

        // 进程起来了就先说一声：面板把自己开出来的时候，就该知道这一格是什么。
        bus.publish(PtyEvent::Started {
            id,
            title: title(shell, command),
        });
        // 订阅要在读线程之前拿到：无论命令多快，`Exited` 都会留在这个接收端里，输入线程一定
        // 见得着它 —— 那是它的退场信号。
        let events = bus.subscribe();

        let output_shared = Arc::clone(&shared);
        let output_bus = bus.clone();
        std::thread::spawn(move || pump_output(id, output_shared, output_bus, reader, child));
        let input_shared = Arc::clone(&shared);
        std::thread::spawn(move || pump_input(id, input_shared, events));

        Ok(Run { id, shared })
    }

    /// 面板不碰进程：按下的键以字节到这里，再写进 pty。进程已经结束的话，这一笔收不收下是内核
    /// 的事（pty 挂掉之后它照样可能返回成功），这里只保证如实转达：`io::Result`，不 panic。
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.shared.write(bytes)
    }

    /// 面板拿到新尺寸时调到这儿。同样地：会话已经结束时它给一个 `io::Result`，不 panic。
    pub fn resize(&mut self, size: (u16, u16)) -> io::Result<()> {
        self.shared.resize(size)
    }

    /// 杀整个进程组。
    pub fn kill(&mut self) {
        self.shared.kill();
    }

    /// 还活着吗。读线程收尾（或者有人 `kill`）之后就是 false。
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    /// 这一格的名字，也是总线上认它的方式。
    pub fn id(&self) -> PaneId {
        self.id
    }
}

impl Drop for Run {
    /// 面板关掉不会留下孤儿：这一格没人看得见了，它的进程组也不该再跑。
    fn drop(&mut self) {
        // 已经结束的会话不必再杀一次 —— 那个 pid 这时候可能已经是别人的进程组了。
        if self.is_running() {
            self.shared.kill();
        }
    }
}

/// 读主端的那条线程：读到多少发多少，读不动了就报一次 `Exited`，然后收工。
///
/// 这条线程从头到尾只做一件事。把输入也塞进来会更省一条线程，但 `read` 是阻塞的、闲着的 shell
/// 不输出，输入就得等下一次打印才动得了。
fn pump_output(
    id: PaneId,
    shared: Arc<Shared>,
    bus: EventBus,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
) {
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => bus.publish(PtyEvent::Output {
                id,
                bytes: buffer[..n].to_vec(),
            }),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // 子进程一死，主端读到的就是 `EIO`：那不是出错，是这一会话说完了最后一句话。
            Err(_) => break,
        }
    }
    // 「结束了」先落地，「退出码」后到：看到 `Exited` 的人也该看到 `is_running` 已经是 false。
    shared.running.store(false, Ordering::Relaxed);
    bus.publish(PtyEvent::Exited {
        id,
        code: exit_code(&mut child),
    });
}

/// 收输入的那条线程：把总线上给本会话的输入、尺寸和「关掉」收进进程里。
///
/// 用 `blocking_recv` 而不是 `try_recv` 轮询：读线程发的那次 `Exited` 就是它的退场信号，会话的
/// 每一种结束方式都会让它出现（谁杀了这个进程组，主端都会读到 `EIO`），所以它不必每几毫秒醒一次
/// 去问「有事吗」。
fn pump_input(id: PaneId, shared: Arc<Shared>, mut events: broadcast::Receiver<Event>) {
    loop {
        match events.blocking_recv() {
            Ok(Event::Pty(PtyEvent::Input { id: to, bytes })) if to == id => {
                // 写不进去只说明进程已经没了，而这件事读线程马上就会说；这里再报一次是噪声。
                let _ = shared.write(&bytes);
            }
            Ok(Event::Pty(PtyEvent::Resize { id: to, rows, cols })) if to == id => {
                let _ = shared.resize((rows, cols));
            }
            Ok(Event::Pty(PtyEvent::Kill { id: to })) if to == id => shared.kill(),
            // 结束了就收工：再往后不会有人给这一格打字了。
            Ok(Event::Pty(PtyEvent::Exited { id: to, .. })) if to == id => break,
            // 别人的事件、别人那一格的输入，都和这里无关。
            Ok(_) => {}
            // 落后了说明这期间的事件已经丢了。进程还活着就继续听；它已经死了就没必要再听了。
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if !shared.running.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// 命令自己退出的给 `Some(code)`，被信号打死的给 `None`：前者是它说完了，后者是有人打断了它。
fn exit_code(child: &mut Box<dyn Child + Send + Sync>) -> Option<i32> {
    let status = match child.try_wait() {
        Ok(Some(status)) => status,
        // 主端已经关了，说明这一端也走完了；`wait` 只是把僵尸收掉，不会久等。
        Ok(None) => child.wait().ok()?,
        Err(_) => return None,
    };
    match status.signal() {
        Some(_) => None,
        None => Some(status.exit_code() as i32),
    }
}

/// 面板标题：交互式 shell 显示 shell 自己的名字，一条命令显示命令的第一行。
fn title(shell: &str, command: &str) -> String {
    if !command.is_empty() {
        return command.lines().next().unwrap_or_default().to_string();
    }
    Path::new(shell)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| shell.to_string())
}

/// `(rows, cols)` → portable-pty 的那套尺寸。
fn pty_size((rows, cols): (u16, u16)) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// 按组杀要的那个 pid。拿不到就没法按组杀，那就在报错之前先把它自己杀了 —— 不能留一个我们追不到
/// 的进程。
#[cfg(unix)]
fn session_pid(child: &mut Box<dyn Child + Send + Sync>) -> io::Result<u32> {
    match child.process_id() {
        Some(pid) => Ok(pid),
        None => {
            let _ = child.kill();
            Err(io::Error::other("pty 子进程没有 pid，按组杀不了"))
        }
    }
}

/// portable-pty 的错都裹在 `anyhow` 里，而这里对外只有 `io::Result`：说清楚是谁失败的就够了。
fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// A pty test needs a pty and a shell to run in it. Both are Unix things here: the tests below start
/// `sh`, and what they are about is how this layer carries a terminal's bytes around, not about how
/// Windows would spell the same idea. See the note in the README.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    /// 本文件里所有等待的上限。pty 事件是另一个线程发的，测试只能等，不能赌。
    const PATIENCE: Duration = Duration::from_secs(5);

    /// 一次会话看到的东西，按到达顺序攒起来。
    #[derive(Default)]
    struct Seen {
        title: Option<String>,
        out: Vec<u8>,
        /// `Some` 表示 `Exited` 已经到过，里面就是退出码（被信号打死的是 `None`）。
        code: Option<Option<i32>>,
    }

    /// 事件说的是哪一格。
    fn pane(event: &PtyEvent) -> PaneId {
        match event {
            PtyEvent::Started { id, .. }
            | PtyEvent::Output { id, .. }
            | PtyEvent::Exited { id, .. }
            | PtyEvent::Input { id, .. }
            | PtyEvent::Resize { id, .. }
            | PtyEvent::Kill { id } => *id,
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// 一个 pty 会话，外加一条把事件搬进 std mpsc 的线程：测试用带超时的 `recv` 等事件，
    /// 而不是等着一个可能永远不来的东西。
    struct Fixture {
        id: PaneId,
        bus: EventBus,
        run: Run,
        events: mpsc::Receiver<Event>,
    }

    impl Fixture {
        fn new(shell: &str, command: &str) -> Self {
            let id = PaneId::new(7);
            let bus = EventBus::new(256);
            let (tx, rx) = mpsc::channel();
            let mut subscription = bus.subscribe();
            // 先订阅再起会话：`Started` 是第一个事件，漏了就补不回来了。
            std::thread::spawn(move || {
                while let Ok(event) = subscription.blocking_recv() {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            });
            let run = Run::spawn(id, shell, command, (24, 80), bus.clone()).expect("一个 pty 会话");
            Self {
                id,
                bus,
                run,
                events: rx,
            }
        }

        /// 属于本格的那个事件，别人的落错地方就是测试自己的错。
        fn pty(&self, event: Event) -> PtyEvent {
            match event {
                Event::Pty(pty) => {
                    assert_eq!(pane(&pty), self.id, "别人的事件落到这一格上了");
                    pty
                }
                other => panic!("不是 pty 事件：{other:?}"),
            }
        }

        /// 收事件，直到 `done` 说够了、或者 `Exited` 到了、或者超过 `within`。
        fn seen(&self, done: impl Fn(&Seen) -> bool, within: Duration) -> Seen {
            let deadline = Instant::now() + within;
            let mut seen = Seen::default();
            loop {
                if done(&seen) {
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                let Ok(event) = self.events.recv_timeout(left) else {
                    break;
                };
                match self.pty(event) {
                    PtyEvent::Started { title, .. } => seen.title = Some(title),
                    PtyEvent::Output { bytes, .. } => seen.out.extend_from_slice(&bytes),
                    PtyEvent::Exited { code, .. } => {
                        seen.code = Some(code);
                        break;
                    }
                    // 面板方向的事件是测试自己发出去的，这里不看。
                    PtyEvent::Input { .. } | PtyEvent::Resize { .. } | PtyEvent::Kill { .. } => {}
                }
            }
            seen
        }

        /// 等输出里出现这句话 —— 会话还没结束时，早到的答案不该让测试再等下去。
        fn wait_for(&self, needle: &str, within: Duration) -> Seen {
            self.seen(|seen| text(&seen.out).contains(needle), within)
        }
    }

    /// 进程组散了吗。不是立刻：被 SIGKILL 掉的那个 `sleep` 还要等人收尸（它的父进程也一起被杀
    /// 了），所以这里给 3 秒余量，而不是断言「此刻就没了」。
    #[cfg(unix)]
    fn group_is_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let probe = unsafe { libc::kill(-pid, 0) };
            let gone =
                probe == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone || Instant::now() >= deadline {
                return gone;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn output_arrives_as_events_and_the_exit_code_closes_the_session() {
        let fixture = Fixture::new("/bin/sh", "printf 'hello pty\\n'");
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(
            seen.title.as_deref(),
            Some("printf 'hello pty\\n'"),
            "标题是命令的第一行"
        );
        assert!(
            text(&seen.out).contains("hello pty"),
            "输出该原样到：{:?}",
            text(&seen.out)
        );
        assert_eq!(seen.code, Some(Some(0)), "自己退出的给它的退出码");
    }

    #[test]
    fn an_empty_command_is_an_interactive_shell_named_after_its_shell() {
        let fixture = Fixture::new("/bin/sh", "");
        let seen = fixture.seen(|seen| seen.title.is_some(), PATIENCE);
        assert_eq!(seen.title.as_deref(), Some("sh"), "空命令没有第一行可看");

        // 顺手证明这个 shell 真的在等输入：给它一个 `exit`，它就该自己走掉。
        fixture.bus.publish(PtyEvent::Input {
            id: fixture.id,
            bytes: b"exit\n".to_vec(),
        });
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(Some(0)));
    }

    #[test]
    fn a_keystroke_typed_at_a_pane_reaches_the_command() {
        let fixture = Fixture::new("/bin/sh", "while read -r line; do echo \"got:$line\"; done");
        // 面板不碰进程：这一次按键先被编码成字节，作为事件回到引擎。
        fixture.bus.publish(PtyEvent::Input {
            id: fixture.id,
            bytes: b"hello\n".to_vec(),
        });
        let seen = fixture.wait_for("got:hello", PATIENCE);
        assert!(
            text(&seen.out).contains("got:hello"),
            "命令自己回的那句话才算收到了输入（终端的回显不算）：{:?}",
            text(&seen.out)
        );
    }

    #[test]
    fn a_resize_event_reaches_the_pty() {
        // 命令慢一拍，好让尺寸先到；`stty` 问的是内核，内核说的才算数。
        let fixture = Fixture::new("/bin/sh", "sleep 0.5; stty size");
        fixture.bus.publish(PtyEvent::Resize {
            id: fixture.id,
            rows: 40,
            cols: 100,
        });
        let seen = fixture.wait_for("40 100", PATIENCE);
        assert!(
            text(&seen.out).contains("40 100"),
            "stty 看到的该是面板给的尺寸，而不是起进程时的那个：{:?}",
            text(&seen.out)
        );
    }

    #[test]
    fn a_command_that_exits_reports_its_code() {
        let fixture = Fixture::new("/bin/sh", "exit 7");
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(Some(7)));
    }

    #[test]
    fn a_command_killed_by_a_signal_reports_no_code() {
        // `$$` 是 shell 自己：它被信号打死了，和「它自己退出了」是两件事。
        let fixture = Fixture::new("/bin/sh", "kill -9 $$");
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(None), "被信号打死的不报退出码");
    }

    #[test]
    fn a_shell_that_does_not_exist_is_an_error_and_not_a_session() {
        let spawned = Run::spawn(
            PaneId::new(1),
            "/nowhere/zsh",
            "",
            (24, 80),
            EventBus::new(8),
        );
        assert!(
            spawned.is_err(),
            "起不来的命令该说一声，而不是给一个没有进程的会话"
        );
    }

    #[cfg(unix)]
    #[test]
    fn killing_a_session_ends_it_and_takes_the_process_group_with_it() {
        // 后台那个 `sleep` 面板从没见过，但它在这一组里，就必须跟着这一格一起走。
        let mut fixture = Fixture::new("/bin/sh", "sleep 30 & echo $!; wait");
        // 等它真的把 `sleep` 起起来：杀一个还没 fork 的 shell 证明不了组杀。
        fixture.seen(|seen| text(&seen.out).contains('\n'), PATIENCE);
        let pid = fixture.run.shared.pid as i32;

        fixture.run.kill();
        assert!(!fixture.run.is_running(), "杀过就不该还在跑");

        // `Exited` 到齐说明读线程也把子进程收掉了，那个 pid 从此不再属于任何进程组。
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(None), "被 SIGKILL 打死的没有退出码");
        assert!(group_is_gone(pid), "进程组里还有人：{pid}");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_session_kills_what_it_started() {
        let fixture = Fixture::new("/bin/sh", "sleep 30 & echo $!; wait");
        // 等它真的把 `sleep` 起起来，不然丢掉的只是一个还没 fork 的 shell。
        fixture.seen(|seen| text(&seen.out).contains('\n'), PATIENCE);
        let pid = fixture.run.shared.pid as i32;

        // 面板关掉就是 `Run` 被丢掉。
        drop(fixture.run);
        assert!(group_is_gone(pid), "丢掉会话留下了孤儿：{pid}");
    }

    #[test]
    fn write_and_resize_after_the_end_are_answered_and_not_panicked() {
        let mut fixture = Fixture::new("/bin/sh", "printf 'bye\\n'");
        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(Some(0)));
        assert!(!fixture.run.is_running());

        // 收不收下这一笔是内核的事 —— pty 挂掉之后写主端在 Linux 上照样返回成功，字节只是进了
        // 一个没人会读的输入队列。这里的约定是：进程已经没了，这两个调用也只能给 `io::Result`，
        // 不能炸，也不能把会话叫回来。
        let _ = fixture.run.write(b"anybody home?\n");
        let _ = fixture.run.resize((40, 100));
        assert!(!fixture.run.is_running(), "写完之后它也不该「在跑」");
    }

    #[test]
    fn a_kill_event_ends_the_session() {
        // 面板关掉了（Ctrl+W）：一个没人看得见的进程，就是没人能停的进程。
        let fixture = Fixture::new("/bin/sh", "sleep 30");
        fixture.bus.publish(PtyEvent::Kill { id: fixture.id });

        let seen = fixture.seen(|_| false, PATIENCE);
        assert_eq!(seen.code, Some(None), "它该结束");
        assert!(!fixture.run.is_running(), "面板说不要了，它就不该还在跑");
    }
}
