// Ported from GBLMX/pigma (Apache-2.0) and adapted for this workspace — see CREDITS.md and
// THIRD_PARTY_LICENSES/ for what changed and the licence text.
//! The Windows half of the background probe: the console's own answer, which is all there is
//! to ask when ConPTY sits in the way of the byte stream.

use super::super::color::rgb_luminance;

/// Ask the Windows console for its background colour.
///
/// There is no `/dev/tty` and no OSC 11 answer to wait for (ConPTY is in the way), but the
/// console API knows the answer: the screen buffer's attributes carry the background palette
/// index and `ColorTable` carries the palette, which Windows Terminal fills in from the color
/// scheme the tab was started with. Best effort — a process without a console fails the call
/// and the caller keeps the dark default.
pub(super) fn probe_console_background() -> Option<f64> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFOEX, GetConsoleScreenBufferInfoEx, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    let mut info = CONSOLE_SCREEN_BUFFER_INFOEX {
        cbSize: std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is an initialised CONSOLE_SCREEN_BUFFER_INFOEX whose `cbSize` the API
    // requires; the handle is stdout, and the call reports failure instead of faulting when
    // that is not a console.
    if unsafe { GetConsoleScreenBufferInfoEx(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } == 0 {
        return None;
    }

    // The low nibble of the attributes is the background: 0-15, an index into `ColorTable`.
    let index = usize::from(info.wAttributes & 0x000F);
    let color = *info.ColorTable.get(index)?;
    Some(console_color_luminance(color))
}

/// `COLORREF` is `0x00BBGGRR` — green in the middle, which is the opposite of the web order.
pub(super) fn console_color_luminance(color: u32) -> f64 {
    let channel = |shift: u32| f64::from((color >> shift) & 0xFF) / 255.0;
    rgb_luminance(channel(0), channel(8), channel(16))
}
