//! Pure-Rust VT100/xterm/OSC-7501 terminal emulator drop-in replacement
//! for `herdr-core`'s `ghostty-vt` binding (zero Zig, zero C FFI, `#![forbid(unsafe_code)]`).

mod keys;
mod parser;
mod width;

use parser::{EmulatorSnapshot, Engine, SNAPSHOT_VERSION};

/// Default foreground/background colors matching Ghostty until a viewer supplies its own.
const DEFAULT_CELL: (u32, u32) = (10, 20);
const DEFAULT_FG: Rgb = [0xff, 0xff, 0xff];
const DEFAULT_BG: Rgb = [0x28, 0x2c, 0x34];

pub type Rgb = [u8; 3];

/// One OSC 7501 report, exactly as the program sent it (strings decoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    /// `idle`, `working`, `done`, `blocked`, `error`, or `clear` (remove records).
    pub state: &'static str,
    /// `permission`, `question`, or `auth`, only when `blocked` and specified.
    pub kind: Option<&'static str>,
    /// `0..=100`, only for `working` and `blocked` when sent.
    pub progress: Option<u8>,
    /// Empty for the root record.
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

/// One parsed terminal state machine with primary/alternate screens, scrollback,
/// display query responses, key/wheel encoders, and OSC 7501 status reports.
pub struct Terminal {
    engine: Engine,
}

impl Terminal {
    /// Creates a new terminal of `cols`x`rows` keeping up to `scrollback_bytes` of scrollback.
    pub fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> Self {
        Self {
            engine: Engine::new(
                cols,
                rows,
                scrollback_bytes,
                DEFAULT_FG,
                DEFAULT_BG,
                DEFAULT_FG,
                DEFAULT_CELL,
            ),
        }
    }

    /// Feeds raw PTY output bytes into the terminal parser and screen buffers.
    pub fn write(&mut self, bytes: &[u8]) {
        self.engine.write(bytes);
    }

    /// Bytes that reconstruct the unfinished parser state; `None` past `CONTINUATION_MAX_BYTES`.
    pub fn continuation(&self) -> Option<Vec<u8>> {
        self.engine.continuation()
    }

    /// Consumes through the first clean parser ground boundary, or returns `None` if reached nowhere.
    pub fn write_until_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        self.engine.write_until_ground(bytes)
    }

    /// Query replies and subscribed notifications queued since the last call.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.engine.responses)
    }

    /// Drops replies made after the first `keep` bytes.
    pub fn drop_responses_after(&mut self, keep: usize) {
        self.engine.responses.truncate(keep);
    }

    /// OSC 7501 status reports received since the last call, in order.
    pub fn take_status_reports(&mut self) -> Vec<StatusReport> {
        std::mem::take(&mut self.engine.statuses)
    }

    /// Whether the window title changed since the last call.
    pub fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.engine.title_changed)
    }

    /// Encodes a key press in the terminal's current keyboard/cursor modes.
    pub fn encode_key(&self, key: i32, mods: u16, text: &str, unshifted: u32) -> Vec<u8> {
        keys::encode_key(
            key,
            mods,
            text,
            unshifted,
            self.dec_mode(1),
            self.engine.kitty_flags(),
        )
    }

    /// Encodes one mouse-wheel notch at the center of the screen if mouse tracking is enabled.
    pub fn wheel(&self, up: bool) -> Vec<u8> {
        let mouse_active = self.dec_mode(1000) || self.dec_mode(1002) || self.dec_mode(1003);
        keys::encode_wheel(
            up,
            self.engine.cols,
            self.engine.rows,
            mouse_active,
            self.dec_mode(1006),
        )
    }

    /// Returns the visible screen rows, each padded to the screen width.
    pub fn screen_rows(&self) -> Vec<String> {
        self.engine.active().screen_rows()
    }

    /// Returns the number of response bytes queued for `take_responses`.
    pub fn pending_responses(&self) -> usize {
        self.engine.responses.len()
    }

    /// Sets the `XTVERSION` string answered by `CSI > q`.
    pub fn set_xtversion(&mut self, version: &str) {
        self.engine.xtversion = version.to_string();
    }

    /// Updates viewer display defaults (foreground, background, cursor RGB, cell pixels).
    pub fn set_display(
        &mut self,
        fg: Option<Rgb>,
        bg: Option<Rgb>,
        cursor: Option<Rgb>,
        cell: Option<(u32, u32)>,
    ) {
        let mut changed = false;
        if let Some(c) = fg {
            changed |= self.engine.fg != c;
            self.engine.fg = c;
        }
        if let Some(c) = bg {
            changed |= self.engine.bg != c;
            self.engine.bg = c;
        }
        if let Some(c) = cursor {
            changed |= self.engine.cursor_color != c;
            self.engine.cursor_color = c;
        }
        if let Some(sz) = cell {
            self.engine.cell = sz;
        }
        if changed && self.dec_mode(2031) {
            let scheme = if self.engine.dark() { 1 } else { 2 };
            let report = format!("\x1b[?997;{scheme}n");
            self.engine.responses.extend_from_slice(report.as_bytes());
        }
    }

    /// Resizes the primary and alternate screen buffers to `cols`x`rows`.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.engine.cols = cols;
        self.engine.rows = rows;
        self.engine.primary.resize(cols, rows);
        self.engine.alternate.resize(cols, rows);
    }

    /// Returns `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.engine.cols, self.engine.rows)
    }

    /// Returns `true` when the alternate screen buffer is active.
    pub fn alt_screen(&self) -> bool {
        self.engine.alt_active
    }

    /// Queries a DEC private mode (e.g. `1004`, `2004`, `2027`).
    pub fn dec_mode(&self, mode: u16) -> bool {
        self.engine.dec_mode(mode)
    }

    /// Returns the terminal's current window title.
    pub fn title(&self) -> String {
        self.engine.title.clone()
    }

    /// Returns scrollback plus the active screen as plain text (soft wraps joined).
    pub fn plain_text(&self) -> String {
        self.engine.active().plain_text(!self.engine.alt_active)
    }

    /// Emits VT bytes that repaint this terminal on a fresh viewer, including up to `history`
    /// scrollback lines (main screen only), current screen contents, modes, cursor, and title.
    pub fn paint(&self, history: usize) -> Vec<u8> {
        let mut out = b"\x1b[!p\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?1l\x1b[?66l\x1b[?2031l\x1b[?2048l\x1b[=0;1u\
\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[H\x1b[2J"
            .to_vec();

        if self.engine.alt_active {
            out.extend_from_slice(&self.engine.primary.render_vt(
                history,
                true,
                self.engine.primary.kitty_stack.last().copied().unwrap_or(0),
            ));
            out.extend_from_slice(b"\x1b[?1049h\x1b[!p");
            out.extend_from_slice(&self.engine.alternate.render_vt(
                0,
                true,
                self.engine.kitty_flags(),
            ));
        } else {
            out.extend_from_slice(&self.engine.primary.render_vt(
                history,
                true,
                self.engine.kitty_flags(),
            ));
        }

        for &mode in &self.engine.dec_modes {
            if matches!(mode, 1 | 6 | 1000 | 1002 | 1003 | 1004 | 1006 | 2004 | 2031) {
                out.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
            }
        }
        if !self.dec_mode(7) {
            out.extend_from_slice(b"\x1b[?7l");
        }
        if !self.dec_mode(25) {
            out.extend_from_slice(b"\x1b[?25l");
        }
        if !self.engine.title.is_empty() {
            out.extend_from_slice(format!("\x1b]2;{}\x1b\\", self.engine.title).as_bytes());
        }
        out
    }

    /// Emits VT bytes of the screen and scrollback with colors, but without modes, cursor, or title.
    pub fn replay(&self) -> Vec<u8> {
        let mut out = self.engine.primary.replay_vt();
        if self.engine.alt_active {
            if !out.is_empty() {
                out.extend_from_slice(b"\x1b[0m\r\n");
            }
            out.extend_from_slice(&self.engine.alternate.replay_vt());
        }
        while out.last().is_some_and(|b| b" \r\n".contains(b)) {
            out.pop();
        }
        out
    }

    /// Encodes a complete binary snapshot including any unfinished escape sequence.
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&self.engine.to_snapshot()).ok()
    }

    /// Restores a snapshot previously produced by [`Self::snapshot`].
    pub fn restore(bytes: &[u8], scrollback_bytes: usize) -> Option<Self> {
        let snap: EmulatorSnapshot = serde_json::from_slice(bytes).ok()?;
        if snap.version != SNAPSHOT_VERSION {
            return None;
        }
        Some(Self {
            engine: Engine::from_snapshot(snap, scrollback_bytes),
        })
    }
}

#[cfg(test)]
mod tests;
