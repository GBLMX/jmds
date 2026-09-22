// Ported from GBLMX/pigma (Apache-2.0) and adapted for this workspace — see CREDITS.md and
// THIRD_PARTY_LICENSES/ for what changed and the licence text.
//! The Unix half of the background probe.
//!
//! A tty is opened and written the OSC 11 query on, and the reply is read back under one
//! deadline — see `query_background_on_tty` for why the read cannot be the ordinary one.

use super::{REPLY_BUDGET, parse_osc11_luminance, read_osc11_reply, write_osc11_query};

/// Ask the controlling terminal for its background color with a bounded wait.
///
/// A terminal that does not implement OSC 11 simply never answers, so poll first and
/// give up after a frame instead of blocking startup.
#[cfg(unix)]
pub(super) fn probe_tty_background() -> Option<f64> {
    use std::fs::OpenOptions;

    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    query_background_on_tty(&tty)
}

/// The probe itself, against an already-open tty.
///
/// Split out so a test can hand it a pty whose other end plays the terminal
/// (`the_background_probe_reads_a_reply_without_a_newline`).
///
/// The reply — `ESC ] 11 ; rgb:… ESC \` — carries **no newline**, so in the canonical mode
/// a tty starts in, the line discipline holds it back and the poll below times out on
/// every terminal that does answer. The probe therefore reads the tty the way the event
/// loop later will: non-canonical, no echo, byte-at-a-time, under one overall deadline.
/// Measured on a pty: canonical never delivers the reply, non-canonical delivers it at once.
#[cfg(unix)]
pub(super) fn query_background_on_tty(tty: &std::fs::File) -> Option<f64> {
    use std::{io::BufReader, os::fd::AsRawFd};

    let _no_stop = NoTtyStopSignals::block();

    let fd = tty.as_raw_fd();
    let _raw = RawTty::new(fd);

    let mut writer = tty.try_clone().ok()?;
    write_osc11_query(&mut writer)?;

    let mut reader = BufReader::new(TtyReaderWithDeadline::new(fd, REPLY_BUDGET));
    read_osc11_reply(&mut reader).and_then(|reply| parse_osc11_luminance(&reply))
}

/// Blocks `SIGTTIN`/`SIGTTOU` for the calling thread, and restores the mask on drop.
///
/// Both are raised by exactly the two things this probe does to a terminal it is not the
/// foreground process group of — reading from it, and changing its modes — and the default
/// action of both is to **stop** the process. A stopped process is a hang with no error,
/// which is the worst outcome a startup probe can produce: it is what stopped the whole test
/// suite the first time this probe ran against a pty it did not own.
///
/// POSIX lets the read and the `tcsetattr` through when the signal is blocked (or ignored) in
/// the calling thread, so blocking is enough — and unlike a process-wide `SIG_IGN` it cannot
/// race with another thread's handler.
#[cfg(unix)]
struct NoTtyStopSignals {
    previous: libc::sigset_t,
}

#[cfg(unix)]
impl NoTtyStopSignals {
    fn block() -> Option<Self> {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: both are valid, writable `sigset_t`s; `sigemptyset`/`sigaddset` initialise
        // `set` before it is read, and `pthread_sigmask` writes `previous` when it returns 0.
        unsafe {
            if libc::sigemptyset(set.as_mut_ptr()) != 0 {
                return None;
            }
            let mut set = set.assume_init();
            if libc::sigaddset(&mut set, libc::SIGTTIN) != 0
                || libc::sigaddset(&mut set, libc::SIGTTOU) != 0
            {
                return None;
            }
            if libc::pthread_sigmask(libc::SIG_BLOCK, &set, previous.as_mut_ptr()) != 0 {
                return None;
            }
            Some(Self {
                previous: previous.assume_init(),
            })
        }
    }
}

#[cfg(unix)]
impl Drop for NoTtyStopSignals {
    fn drop(&mut self) {
        // SAFETY: `self.previous` is the mask `pthread_sigmask` filled in on creation.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

/// A tty reader that gives up once `deadline` passes.
///
/// `read_osc11_reply` reads byte by byte, so the wait has to be bounded per byte *and* in
/// total: the poll before each read waits for the time that is left, and once nothing is
/// left the read fails — which the reply parser turns into "no answer".
#[cfg(unix)]
struct TtyReaderWithDeadline {
    fd: std::os::fd::RawFd,
    deadline: std::time::Instant,
}

#[cfg(unix)]
impl TtyReaderWithDeadline {
    fn new(fd: std::os::fd::RawFd, budget: std::time::Duration) -> Self {
        Self {
            fd,
            deadline: std::time::Instant::now() + budget,
        }
    }
}

#[cfg(unix)]
impl std::io::Read for TtyReaderWithDeadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no OSC 11 reply within the budget",
            ));
        }

        let mut poll_fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a valid, initialised pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, remaining.as_millis() as libc::c_int) };
        if ready <= 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no OSC 11 reply within the budget",
            ));
        }

        // SAFETY: `buf` is a valid, writable slice of `buf.len()` bytes and `self.fd` is an
        // open tty for as long as the caller holds it.
        let count = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(count as usize)
    }
}

/// Put the controlling terminal into the mode the probe needs, and put it back on drop.
///
/// Only canonical input buffering and echo change: the probe runs before the UI takes the
/// terminal over, so everything else is left exactly as the user had it, and the previous
/// settings are restored even when the probe gives up early.
#[cfg(unix)]
struct RawTty {
    fd: std::os::fd::RawFd,
    saved: libc::termios,
}

#[cfg(unix)]
impl RawTty {
    fn new(fd: std::os::fd::RawFd) -> Option<Self> {
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `saved` is a valid pointer to writable memory for one `termios`.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `tcgetattr` returned 0, so it initialised `saved`.
        let saved = unsafe { saved.assume_init() };

        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `fd` is an open tty and `raw` is an initialised `termios`.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }

        Some(Self { fd, saved })
    }
}

#[cfg(unix)]
impl Drop for RawTty {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is the tty this guard was created for and `self.saved` is the
        // `termios` it had at that moment.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}
