//! VT100/xterm screen buffer, ANSI/OSC/CSI/DCS parser, and OSC 7501 status decoder.

use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::StatusReport;
use crate::width::char_width;

/// Unfinished escape sequences kept so a snapshot can be taken at any byte.
pub(crate) const CONTINUATION_MAX_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_CSI_PARAMS: usize = 32;

const ATTR_BOLD: u16 = 1 << 0;
const ATTR_DIM: u16 = 1 << 1;
const ATTR_ITALIC: u16 = 1 << 2;
const ATTR_UNDERLINE: u16 = 1 << 3;
const ATTR_BLINK: u16 = 1 << 4;
const ATTR_INVERSE: u16 = 1 << 5;
const ATTR_HIDDEN: u16 = 1 << 6;
const ATTR_STRIKE: u16 = 1 << 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct Style {
    pub attrs: u16,
    pub fg: Color,
    pub bg: Color,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cell {
    pub text: String,
    pub width: u8,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Style::default())
    }
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            text: " ".to_string(),
            width: 1,
            style,
        }
    }

    fn continuation(style: Style) -> Self {
        Self {
            text: String::new(),
            width: 0,
            style,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Row {
    pub cells: Vec<Cell>,
    pub soft_wrapped: bool,
}

impl Row {
    fn blank(cols: usize, style: Style) -> Self {
        Self {
            cells: vec![Cell::blank(style); cols],
            soft_wrapped: false,
        }
    }

    fn plain_trimmed(&self) -> String {
        let mut s = String::with_capacity(self.cells.len());
        for cell in &self.cells {
            if cell.width > 0 {
                if cell.text.is_empty() {
                    s.push(' ');
                } else {
                    s.push_str(&cell.text);
                }
            }
        }
        s.trim_end_matches(' ').to_string()
    }

    fn padded_text(&self, cols: usize) -> String {
        let mut s = String::with_capacity(cols);
        let mut used = 0usize;
        for cell in &self.cells {
            if used >= cols {
                break;
            }
            if cell.width == 0 {
                continue;
            }
            let w = usize::from(cell.width);
            if used + w > cols {
                break;
            }
            if cell.text.is_empty() {
                s.push(' ');
            } else {
                s.push_str(&cell.text);
            }
            used += w;
        }
        while used < cols {
            s.push(' ');
            used += 1;
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct CursorState {
    pub col: usize,
    pub row: usize,
    pub style: Style,
    pub origin_mode: bool,
    pub wrap_next: bool,
    pub g0_line_drawing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BufferState {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<Row>,
    pub scrollback: VecDeque<Row>,
    pub max_scrollback_bytes: usize,
    pub scrollback_bytes: usize,
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    pub cursor: CursorState,
    pub saved_cursor: Option<CursorState>,
    pub kitty_stack: Vec<u8>,
}

impl BufferState {
    pub(crate) fn new(cols: u16, rows: u16, max_scrollback_bytes: usize) -> Self {
        let cols = usize::from(cols.max(1));
        let rows = usize::from(rows.max(1));
        Self {
            cols,
            rows,
            lines: (0..rows)
                .map(|_| Row::blank(cols, Style::default()))
                .collect(),
            scrollback: VecDeque::new(),
            max_scrollback_bytes,
            scrollback_bytes: 0,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            cursor: CursorState::default(),
            saved_cursor: None,
            kitty_stack: Vec::new(),
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback_bytes == 0 {
            return;
        }
        let approx = row.cells.len() * 8;
        self.scrollback.push_back(row);
        self.scrollback_bytes = self.scrollback_bytes.saturating_add(approx);
        while self.scrollback_bytes > self.max_scrollback_bytes && self.scrollback.len() > 1 {
            if let Some(dropped) = self.scrollback.pop_front() {
                self.scrollback_bytes = self
                    .scrollback_bytes
                    .saturating_sub(dropped.cells.len() * 8);
            }
        }
    }

    fn reset_margins(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    pub(crate) fn resize(&mut self, new_cols: u16, new_rows: u16) {
        let new_cols = usize::from(new_cols.max(1));
        let new_rows = usize::from(new_rows.max(1));
        for row in &mut self.lines {
            resize_row(row, new_cols);
        }
        while self.lines.len() < new_rows {
            self.lines.push(Row::blank(new_cols, Style::default()));
        }
        while self.lines.len() > new_rows {
            let excess = self.lines.remove(0);
            self.push_scrollback(excess);
        }
        self.cols = new_cols;
        self.rows = new_rows;
        self.reset_margins();
        self.cursor.col = self.cursor.col.min(new_cols.saturating_sub(1));
        self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
        self.cursor.wrap_next = false;
    }

    fn scroll_up_region(&mut self, top: usize, bottom: usize, n: usize, record_scrollback: bool) {
        if top >= self.rows || bottom >= self.rows || top > bottom || n == 0 {
            return;
        }
        let region_len = bottom - top + 1;
        let steps = n.min(region_len);
        let fill = Style {
            bg: self.cursor.style.bg,
            ..Style::default()
        };
        for _ in 0..steps {
            let evicted = self.lines.remove(top);
            if record_scrollback && top == 0 {
                self.push_scrollback(evicted);
            }
            self.lines.insert(bottom, Row::blank(self.cols, fill));
        }
    }

    fn scroll_down_region(&mut self, top: usize, bottom: usize, n: usize) {
        if top >= self.rows || bottom >= self.rows || top > bottom || n == 0 {
            return;
        }
        let region_len = bottom - top + 1;
        let steps = n.min(region_len);
        let fill = Style {
            bg: self.cursor.style.bg,
            ..Style::default()
        };
        for _ in 0..steps {
            self.lines.remove(bottom);
            self.lines.insert(top, Row::blank(self.cols, fill));
        }
    }

    fn newline(&mut self, record_scrollback: bool) {
        self.cursor.wrap_next = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up_region(self.scroll_top, self.scroll_bottom, 1, record_scrollback);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.wrap_next = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_down_region(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn put_grapheme(&mut self, ch: char, auto_wrap: bool, record_scrollback: bool) {
        let ch = if self.cursor.g0_line_drawing {
            map_dec_special_graphics(ch)
        } else {
            ch
        };
        let w = char_width(ch);
        if w == 0 {
            if ch == '\u{fe0f}' || crate::width::is_combining(ch) {
                self.append_combining(ch);
            }
            return;
        }
        if self.cursor.wrap_next && auto_wrap {
            if let Some(row) = self.lines.get_mut(self.cursor.row) {
                row.soft_wrapped = true;
            }
            self.cursor.col = 0;
            self.newline(record_scrollback);
        }
        if w == 2 && self.cols > 1 && self.cursor.col == self.cols - 1 {
            if let Some(row) = self.lines.get_mut(self.cursor.row) {
                row.cells[self.cursor.col] = Cell::blank(self.cursor.style);
                if auto_wrap {
                    row.soft_wrapped = true;
                }
            }
            if auto_wrap {
                self.cursor.col = 0;
                self.newline(record_scrollback);
            } else {
                return;
            }
        }
        let col = self.cursor.col.min(self.cols.saturating_sub(1));
        let style = self.cursor.style;
        let row = &mut self.lines[self.cursor.row];
        clear_wide_overlap(row, col);
        let width = u8::try_from(w.min(self.cols.saturating_sub(col))).unwrap_or(1);
        row.cells[col] = Cell {
            text: ch.to_string(),
            width,
            style,
        };
        if width == 2 && col + 1 < self.cols {
            clear_wide_overlap(row, col + 1);
            row.cells[col + 1] = Cell::continuation(style);
        }
        let advance = usize::from(width);
        if col + advance >= self.cols {
            self.cursor.col = self.cols - 1;
            if auto_wrap {
                self.cursor.wrap_next = true;
            }
        } else {
            self.cursor.col = col + advance;
            self.cursor.wrap_next = false;
        }
    }

    fn append_combining(&mut self, ch: char) {
        let target_col = if self.cursor.wrap_next {
            self.cursor.col
        } else if self.cursor.col > 0 {
            self.cursor.col - 1
        } else {
            return;
        };
        let Some(row) = self.lines.get_mut(self.cursor.row) else {
            return;
        };
        let mut idx = target_col.min(row.cells.len().saturating_sub(1));
        if row.cells[idx].width == 0 && idx > 0 {
            idx -= 1;
        }
        row.cells[idx].text.push(ch);
    }

    pub(crate) fn screen_rows(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|r| r.padded_text(self.cols))
            .collect()
    }

    pub(crate) fn plain_text(&self, include_scrollback: bool) -> String {
        let mut combined: Vec<&Row> = Vec::new();
        if include_scrollback {
            combined.extend(self.scrollback.iter());
        }
        combined.extend(self.lines.iter());
        let mut last_non_empty = 0usize;
        for (idx, row) in combined.iter().enumerate() {
            if !row.plain_trimmed().is_empty() {
                last_non_empty = idx + 1;
            }
        }
        let mut out = String::new();
        let mut i = 0usize;
        while i < last_non_empty {
            let mut line = combined[i].plain_trimmed();
            while combined[i].soft_wrapped && i + 1 < last_non_empty {
                i += 1;
                line.push_str(&combined[i].plain_trimmed());
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line.trim_end_matches(' '));
            i += 1;
        }
        out
    }

    pub(crate) fn render_vt(
        &self,
        history_lines: usize,
        include_extras: bool,
        kitty: u8,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if history_lines > 0 && !self.scrollback.is_empty() {
            let available = self.scrollback.len();
            let start = available.saturating_sub(history_lines);
            let mut style = Style::default();
            for row in self.scrollback.iter().skip(start) {
                append_styled_row(&mut out, row, &mut style);
                out.extend_from_slice(b"\r\n");
            }
            if style != Style::default() {
                out.extend_from_slice(b"\x1b[0m");
            }
            out.extend_from_slice(b"\r\n");
            out.extend(std::iter::repeat_n(b'\n', self.rows));
            out.extend_from_slice(b"\x1b[H\x1b[2J");
        }
        render_grid_vt(
            &mut out,
            &self.lines,
            self.rows,
            &self.cursor,
            self.scroll_top,
            self.scroll_bottom,
            include_extras,
            kitty,
        );
        out
    }

    pub(crate) fn replay_vt(&self) -> Vec<u8> {
        let mut rows: Vec<&Row> = self.scrollback.iter().collect();
        rows.extend(self.lines.iter());
        let mut last_non_empty = 0usize;
        for (idx, row) in rows.iter().enumerate() {
            if !row.plain_trimmed().is_empty() {
                last_non_empty = idx + 1;
            }
        }
        let mut out = Vec::new();
        let mut style = Style::default();
        for (idx, row) in rows.iter().take(last_non_empty).enumerate() {
            if idx > 0 {
                out.extend_from_slice(b"\r\n");
            }
            append_styled_row(&mut out, row, &mut style);
        }
        if style != Style::default() {
            out.extend_from_slice(b"\x1b[0m");
        }
        out
    }
}

#[allow(clippy::too_many_arguments)]
fn render_grid_vt(
    out: &mut Vec<u8>,
    lines: &[Row],
    rows: usize,
    cursor: &CursorState,
    scroll_top: usize,
    scroll_bottom: usize,
    include_extras: bool,
    kitty: u8,
) {
    let mut style = Style::default();
    for (idx, row) in lines.iter().enumerate() {
        if idx > 0 {
            out.extend_from_slice(b"\r\n");
        }
        append_styled_row(out, row, &mut style);
    }
    if include_extras {
        if scroll_top != 0 || scroll_bottom + 1 != rows {
            let _ = write_fmt_bytes(
                out,
                format_args!("\x1b[{};{}r", scroll_top + 1, scroll_bottom + 1),
            );
        }
        if cursor.g0_line_drawing {
            out.extend_from_slice(b"\x1b(0");
        } else {
            out.extend_from_slice(b"\x1b(B");
        }
        if kitty > 0 {
            let _ = write_fmt_bytes(out, format_args!("\x1b[>{kitty}u"));
        }
    }
    emit_sgr_transition(out, &style, &cursor.style);
    if !include_extras && cursor.style != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    if include_extras {
        let _ = write_fmt_bytes(
            out,
            format_args!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1),
        );
    }
}

fn write_fmt_bytes(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) -> std::fmt::Result {
    let mut s = String::new();
    s.write_fmt(args)?;
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn append_styled_row(out: &mut Vec<u8>, row: &Row, active: &mut Style) {
    let mut last_rendered = 0usize;
    for (idx, cell) in row.cells.iter().enumerate() {
        if (cell.width > 0 && cell.text != " " && !cell.text.is_empty())
            || cell.style != Style::default()
        {
            last_rendered = idx + 1;
        }
    }
    for cell in row.cells.iter().take(last_rendered) {
        if cell.width == 0 {
            continue;
        }
        if cell.style != *active {
            emit_sgr_transition(out, active, &cell.style);
            *active = cell.style;
        }
        if cell.text.is_empty() {
            out.push(b' ');
        } else {
            out.extend_from_slice(cell.text.as_bytes());
        }
    }
    if *active != Style::default() && active.bg != Color::Default {
        out.extend_from_slice(b"\x1b[0m");
        *active = Style::default();
    }
}

fn emit_sgr_transition(out: &mut Vec<u8>, from: &Style, to: &Style) {
    if from == to {
        return;
    }
    if *to == Style::default() {
        out.extend_from_slice(b"\x1b[0m");
        return;
    }
    out.extend_from_slice(b"\x1b[0");
    if to.attrs & ATTR_BOLD != 0 {
        out.extend_from_slice(b";1");
    }
    if to.attrs & ATTR_DIM != 0 {
        out.extend_from_slice(b";2");
    }
    if to.attrs & ATTR_ITALIC != 0 {
        out.extend_from_slice(b";3");
    }
    if to.attrs & ATTR_UNDERLINE != 0 {
        out.extend_from_slice(b";4");
    }
    if to.attrs & ATTR_BLINK != 0 {
        out.extend_from_slice(b";5");
    }
    if to.attrs & ATTR_INVERSE != 0 {
        out.extend_from_slice(b";7");
    }
    if to.attrs & ATTR_HIDDEN != 0 {
        out.extend_from_slice(b";8");
    }
    if to.attrs & ATTR_STRIKE != 0 {
        out.extend_from_slice(b";9");
    }
    emit_sgr_color(out, true, to.fg);
    emit_sgr_color(out, false, to.bg);
    out.push(b'm');
}

fn emit_sgr_color(out: &mut Vec<u8>, is_fg: bool, color: Color) {
    match color {
        Color::Default => {}
        Color::Indexed(n) => {
            if n < 8 {
                let code = if is_fg { 30 + n } else { 40 + n };
                let _ = write_fmt_bytes(out, format_args!(";{code}"));
            } else if n < 16 {
                let code = if is_fg { 90 + (n - 8) } else { 100 + (n - 8) };
                let _ = write_fmt_bytes(out, format_args!(";{code}"));
            } else {
                let prefix = if is_fg { 38 } else { 48 };
                let _ = write_fmt_bytes(out, format_args!(";{prefix};5;{n}"));
            }
        }
        Color::Rgb(r, g, b) => {
            let prefix = if is_fg { 38 } else { 48 };
            let _ = write_fmt_bytes(out, format_args!(";{prefix};2;{r};{g};{b}"));
        }
    }
}

fn resize_row(row: &mut Row, new_cols: usize) {
    if row.cells.len() < new_cols {
        row.cells.resize(new_cols, Cell::default());
    } else if row.cells.len() > new_cols {
        row.cells.truncate(new_cols);
    }
}

fn clear_wide_overlap(row: &mut Row, col: usize) {
    if col >= row.cells.len() {
        return;
    }
    if row.cells[col].width == 2 && col + 1 < row.cells.len() {
        row.cells[col + 1] = Cell::blank(row.cells[col + 1].style);
    } else if row.cells[col].width == 0 && col > 0 {
        row.cells[col - 1] = Cell::blank(row.cells[col - 1].style);
    }
}

fn map_dec_special_graphics(ch: char) -> char {
    match ch {
        '`' => '◆',
        'a' => '▒',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => ch,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum ParseState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate(u8),
    Csi,
    Osc,
    OscEsc,
    StringBody,
    StringEsc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EmulatorSnapshot {
    pub version: u32,
    pub cols: u16,
    pub rows: u16,
    pub alt_active: bool,
    pub alt_mode: u16,
    pub primary: BufferState,
    pub alternate: BufferState,
    pub dec_modes: BTreeSet<u16>,
    pub title: String,
    pub xtversion: String,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor_color: [u8; 3],
    pub cell: (u32, u32),
    pub parse_state: ParseState,
    pub raw_seq: Vec<u8>,
    pub utf8_buf: Vec<u8>,
    pub pending_bytes: Vec<u8>,
    pub continuation_overflowed: bool,
}

pub(crate) const SNAPSHOT_VERSION: u32 = 1;

pub(crate) struct Engine {
    pub cols: u16,
    pub rows: u16,
    pub alt_active: bool,
    pub alt_mode: u16,
    pub primary: BufferState,
    pub alternate: BufferState,
    pub dec_modes: BTreeSet<u16>,
    pub title: String,
    pub title_changed: bool,
    pub xtversion: String,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor_color: [u8; 3],
    pub cell: (u32, u32),
    pub responses: Vec<u8>,
    pub statuses: Vec<StatusReport>,
    pub parse_state: ParseState,
    pub raw_seq: Vec<u8>,
    pub utf8_buf: Vec<u8>,
    pub pending_bytes: Vec<u8>,
    pub continuation_overflowed: bool,
}

impl Engine {
    pub(crate) fn new(
        cols: u16,
        rows: u16,
        scrollback_bytes: usize,
        fg: [u8; 3],
        bg: [u8; 3],
        cursor_color: [u8; 3],
        cell: (u32, u32),
    ) -> Self {
        let mut dec_modes = BTreeSet::new();
        dec_modes.insert(7);
        dec_modes.insert(25);
        dec_modes.insert(2027);
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            alt_active: false,
            alt_mode: 0,
            primary: BufferState::new(cols, rows, scrollback_bytes),
            alternate: BufferState::new(cols, rows, 0),
            dec_modes,
            title: String::new(),
            title_changed: false,
            xtversion: String::new(),
            fg,
            bg,
            cursor_color,
            cell,
            responses: Vec::new(),
            statuses: Vec::new(),
            parse_state: ParseState::Ground,
            raw_seq: Vec::new(),
            utf8_buf: Vec::new(),
            pending_bytes: Vec::new(),
            continuation_overflowed: false,
        }
    }

    pub(crate) fn from_snapshot(snap: EmulatorSnapshot, scrollback_bytes: usize) -> Self {
        let mut primary = snap.primary;
        primary.max_scrollback_bytes = scrollback_bytes;
        Self {
            cols: snap.cols,
            rows: snap.rows,
            alt_active: snap.alt_active,
            alt_mode: snap.alt_mode,
            primary,
            alternate: snap.alternate,
            dec_modes: snap.dec_modes,
            title: snap.title,
            title_changed: false,
            xtversion: snap.xtversion,
            fg: snap.fg,
            bg: snap.bg,
            cursor_color: snap.cursor_color,
            cell: snap.cell,
            responses: Vec::new(),
            statuses: Vec::new(),
            parse_state: snap.parse_state,
            raw_seq: snap.raw_seq,
            utf8_buf: snap.utf8_buf,
            pending_bytes: snap.pending_bytes,
            continuation_overflowed: snap.continuation_overflowed,
        }
    }

    pub(crate) fn to_snapshot(&self) -> EmulatorSnapshot {
        EmulatorSnapshot {
            version: SNAPSHOT_VERSION,
            cols: self.cols,
            rows: self.rows,
            alt_active: self.alt_active,
            alt_mode: self.alt_mode,
            primary: self.primary.clone(),
            alternate: self.alternate.clone(),
            dec_modes: self.dec_modes.clone(),
            title: self.title.clone(),
            xtversion: self.xtversion.clone(),
            fg: self.fg,
            bg: self.bg,
            cursor_color: self.cursor_color,
            cell: self.cell,
            parse_state: self.parse_state.clone(),
            raw_seq: self.raw_seq.clone(),
            utf8_buf: self.utf8_buf.clone(),
            pending_bytes: self.pending_bytes.clone(),
            continuation_overflowed: self.continuation_overflowed,
        }
    }

    pub(crate) fn active(&self) -> &BufferState {
        if self.alt_active {
            &self.alternate
        } else {
            &self.primary
        }
    }

    pub(crate) fn active_mut(&mut self) -> &mut BufferState {
        if self.alt_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    pub(crate) fn dark(&self) -> bool {
        let [r, g, b] = self.bg;
        (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000 < 128
    }

    pub(crate) fn kitty_flags(&self) -> u8 {
        self.active().kitty_stack.last().copied().unwrap_or(0)
    }

    pub(crate) fn dec_mode(&self, mode: u16) -> bool {
        self.dec_modes.contains(&(mode & 0x7fff))
    }

    pub(crate) fn continuation(&self) -> Option<Vec<u8>> {
        if self.continuation_overflowed {
            None
        } else {
            Some(self.pending_bytes.clone())
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push_byte(b);
        }
    }

    pub(crate) fn write_until_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        for (idx, &b) in bytes.iter().enumerate() {
            self.push_byte(b);
            if matches!(self.parse_state, ParseState::Ground) && self.utf8_buf.is_empty() {
                return Some(idx + 1);
            }
        }
        None
    }

    fn record_pending(&mut self, b: u8) {
        if self.continuation_overflowed {
            return;
        }
        if self.pending_bytes.len() >= CONTINUATION_MAX_BYTES {
            self.continuation_overflowed = true;
            self.pending_bytes.clear();
        } else {
            self.pending_bytes.push(b);
        }
    }

    fn clear_pending(&mut self) {
        self.pending_bytes.clear();
        self.continuation_overflowed = false;
    }

    fn cancel_escape_to_ground(&mut self) {
        self.parse_state = ParseState::Ground;
        self.raw_seq.clear();
        self.clear_pending();
    }

    fn push_byte(&mut self, b: u8) {
        match self.parse_state {
            ParseState::Ground => self.push_ground_byte(b),
            ParseState::Escape => self.push_escape_byte(b),
            ParseState::EscapeIntermediate(inter) => self.push_escape_intermediate_byte(inter, b),
            ParseState::Csi => self.push_csi_byte(b),
            ParseState::Osc => self.push_osc_byte(b),
            ParseState::OscEsc => self.push_osc_esc_byte(b),
            ParseState::StringBody => self.push_string_byte(b),
            ParseState::StringEsc => self.push_string_esc_byte(b),
        }
    }

    fn push_ground_byte(&mut self, b: u8) {
        if b == 0x1b {
            self.utf8_buf.clear();
            self.clear_pending();
            self.parse_state = ParseState::Escape;
            self.raw_seq.clear();
            self.record_pending(b);
            return;
        }
        if b == 0x18 || b == 0x1a {
            self.utf8_buf.clear();
            self.cancel_escape_to_ground();
            return;
        }
        if !self.utf8_buf.is_empty() {
            if (b & 0xc0) == 0x80 {
                self.utf8_buf.push(b);
                self.record_pending(b);
                if let Ok(s) = std::str::from_utf8(&self.utf8_buf) {
                    let chars: Vec<char> = s.chars().collect();
                    self.utf8_buf.clear();
                    self.clear_pending();
                    for ch in chars {
                        self.emit_ground_char(ch);
                    }
                } else if self.utf8_buf.len() >= 4 {
                    self.utf8_buf.clear();
                    self.clear_pending();
                }
                return;
            }
            self.utf8_buf.clear();
            self.clear_pending();
        }
        if b < 0x80 {
            self.emit_ground_char(char::from(b));
        } else if (b & 0xc0) != 0x80 && utf8_char_len(b).is_some() {
            self.utf8_buf.clear();
            self.utf8_buf.push(b);
            self.clear_pending();
            self.record_pending(b);
        }
    }

    fn emit_ground_char(&mut self, ch: char) {
        let auto_wrap = self.dec_mode(7);
        let alt = self.alt_active;
        match ch {
            '\x00' | '\x07' => {}
            '\x08' => {
                let buf = self.active_mut();
                buf.cursor.wrap_next = false;
                buf.cursor.col = buf.cursor.col.saturating_sub(1);
            }
            '\t' => {
                let buf = self.active_mut();
                buf.cursor.wrap_next = false;
                let next = ((buf.cursor.col / 8) + 1) * 8;
                buf.cursor.col = next.min(buf.cols.saturating_sub(1));
            }
            '\n' | '\x0b' | '\x0c' => {
                self.active_mut().newline(!alt);
            }
            '\r' => {
                let buf = self.active_mut();
                buf.cursor.wrap_next = false;
                buf.cursor.col = 0;
            }
            c if c.is_control() => {}
            c => {
                self.active_mut().put_grapheme(c, auto_wrap, !alt);
            }
        }
    }

    fn push_escape_byte(&mut self, b: u8) {
        self.record_pending(b);
        match b {
            b'[' => {
                self.parse_state = ParseState::Csi;
                self.raw_seq.clear();
            }
            b']' => {
                self.parse_state = ParseState::Osc;
                self.raw_seq.clear();
            }
            b'P' | b'_' | b'^' | b'X' => {
                self.parse_state = ParseState::StringBody;
                self.raw_seq.clear();
            }
            b'(' | b')' | b'*' | b'+' | b'#' => {
                self.parse_state = ParseState::EscapeIntermediate(b);
            }
            b'7' => {
                let buf = self.active_mut();
                buf.saved_cursor = Some(buf.cursor);
                self.cancel_escape_to_ground();
            }
            b'8' => {
                let buf = self.active_mut();
                if let Some(saved) = buf.saved_cursor {
                    buf.cursor = saved;
                    buf.cursor.col = buf.cursor.col.min(buf.cols.saturating_sub(1));
                    buf.cursor.row = buf.cursor.row.min(buf.rows.saturating_sub(1));
                }
                self.cancel_escape_to_ground();
            }
            b'D' => {
                let alt = self.alt_active;
                self.active_mut().newline(!alt);
                self.cancel_escape_to_ground();
            }
            b'E' => {
                let alt = self.alt_active;
                let buf = self.active_mut();
                buf.cursor.col = 0;
                buf.newline(!alt);
                self.cancel_escape_to_ground();
            }
            b'M' => {
                self.active_mut().reverse_index();
                self.cancel_escape_to_ground();
            }
            b'c' => {
                self.hard_reset();
                self.cancel_escape_to_ground();
            }
            0x1b => {
                self.clear_pending();
                self.record_pending(0x1b);
            }
            _ => {
                self.cancel_escape_to_ground();
            }
        }
    }

    fn push_escape_intermediate_byte(&mut self, inter: u8, b: u8) {
        self.record_pending(b);
        if inter == b'(' {
            self.active_mut().cursor.g0_line_drawing = b == b'0';
        }
        self.cancel_escape_to_ground();
    }

    fn push_csi_byte(&mut self, b: u8) {
        if b == 0x18 || b == 0x1a {
            self.cancel_escape_to_ground();
            return;
        }
        if b == 0x1b {
            self.clear_pending();
            self.parse_state = ParseState::Escape;
            self.raw_seq.clear();
            self.record_pending(b);
            return;
        }
        self.record_pending(b);
        if (0x40..=0x7e).contains(&b) {
            let seq = std::mem::take(&mut self.raw_seq);
            self.dispatch_csi(&seq, b);
            self.cancel_escape_to_ground();
        } else if self.raw_seq.len() < 256 {
            self.raw_seq.push(b);
        }
    }

    fn push_osc_byte(&mut self, b: u8) {
        if b == 0x18 || b == 0x1a {
            self.cancel_escape_to_ground();
            return;
        }
        if b == 0x07 {
            let raw = std::mem::take(&mut self.raw_seq);
            self.dispatch_osc(&raw);
            self.cancel_escape_to_ground();
            return;
        }
        self.record_pending(b);
        if b == 0x1b {
            self.parse_state = ParseState::OscEsc;
        } else if self.raw_seq.len() < MAX_LINE_BYTES {
            self.raw_seq.push(b);
        }
    }

    fn push_osc_esc_byte(&mut self, b: u8) {
        self.record_pending(b);
        if b == b'\\' {
            let raw = std::mem::take(&mut self.raw_seq);
            self.dispatch_osc(&raw);
            self.cancel_escape_to_ground();
        } else if b == 0x18 || b == 0x1a {
            self.cancel_escape_to_ground();
        } else {
            self.parse_state = ParseState::Osc;
            if self.raw_seq.len() < MAX_LINE_BYTES {
                self.raw_seq.push(b);
            }
        }
    }

    fn push_string_byte(&mut self, b: u8) {
        if b == 0x18 || b == 0x1a {
            self.cancel_escape_to_ground();
            return;
        }
        self.record_pending(b);
        if b == 0x1b {
            self.parse_state = ParseState::StringEsc;
        }
    }

    fn push_string_esc_byte(&mut self, b: u8) {
        self.record_pending(b);
        if b == b'\\' || b == 0x18 || b == 0x1a {
            self.cancel_escape_to_ground();
        } else if b != 0x1b {
            self.parse_state = ParseState::StringBody;
        }
    }

    fn hard_reset(&mut self) {
        let keep_2027 = self.dec_mode(2027);
        let sb = self.primary.max_scrollback_bytes;
        self.primary = BufferState::new(self.cols, self.rows, sb);
        self.alternate = BufferState::new(self.cols, self.rows, 0);
        self.alt_active = false;
        self.alt_mode = 0;
        self.dec_modes.clear();
        self.dec_modes.insert(7);
        self.dec_modes.insert(25);
        if keep_2027 {
            self.dec_modes.insert(2027);
        }
        self.title.clear();
        self.title_changed = false;
    }

    fn soft_reset(&mut self) {
        let alt = self.alt_active;
        let buf = self.active_mut();
        buf.cursor.style = Style::default();
        buf.cursor.origin_mode = false;
        buf.cursor.wrap_next = false;
        buf.cursor.g0_line_drawing = false;
        buf.saved_cursor = None;
        buf.reset_margins();
        self.dec_modes.insert(7);
        self.dec_modes.insert(25);
        if !alt {
            self.primary.scroll_top = 0;
            self.primary.scroll_bottom = self.primary.rows.saturating_sub(1);
        }
    }

    fn enter_alt_screen(&mut self, mode: u16) {
        self.alt_mode = mode;
        if mode == 1049 {
            self.primary.saved_cursor = Some(self.primary.cursor);
        }
        if !self.alt_active {
            self.alt_active = true;
            if matches!(mode, 1047 | 1049) {
                self.alternate = BufferState::new(self.cols, self.rows, 0);
            }
        }
        self.dec_modes.insert(mode);
    }

    fn leave_alt_screen(&mut self, mode: u16) {
        self.dec_modes.remove(&47);
        self.dec_modes.remove(&1047);
        self.dec_modes.remove(&1049);
        if self.alt_active {
            self.alt_active = false;
            self.alt_mode = 0;
            if mode == 1049
                && let Some(saved) = self.primary.saved_cursor
            {
                self.primary.cursor = saved;
                self.primary.cursor.col = self.primary.cursor.col.min(self.primary.cols - 1);
                self.primary.cursor.row = self.primary.cursor.row.min(self.primary.rows - 1);
            }
        }
    }

    fn dispatch_csi(&mut self, raw: &[u8], final_byte: u8) {
        let s = String::from_utf8_lossy(raw);
        let (prefix, body) = match s.as_bytes().first().copied() {
            Some(p @ (b'?' | b'>' | b'=' | b'<')) => (Some(p), &s[1..]),
            _ => (None, s.as_ref()),
        };
        let (body, inter) = match body.as_bytes().last().copied() {
            Some(i @ (b'!' | b'$' | b' ')) => (&body[..body.len() - 1], Some(i)),
            _ => (body, None),
        };
        let params: Vec<u16> = if body.is_empty() {
            Vec::new()
        } else {
            body.split(';')
                .take(MAX_CSI_PARAMS)
                .map(|part| {
                    part.split(':')
                        .next()
                        .unwrap_or("")
                        .parse::<u16>()
                        .unwrap_or(0)
                })
                .collect()
        };
        let p0 = params.first().copied().unwrap_or(0);
        let p1 = params.get(1).copied().unwrap_or(0);

        match (prefix, inter, final_byte) {
            (None, Some(b'!'), b'p') => self.soft_reset(),
            (Some(b'?'), Some(b'$'), b'p') => {
                let val = if self.dec_mode(p0) { 1 } else { 2 };
                let _ = write_fmt_bytes(&mut self.responses, format_args!("\x1b[?{p0};{val}$y"));
            }
            (Some(b'?'), None, b'h') => self.set_dec_modes(&params, true),
            (Some(b'?'), None, b'l') => self.set_dec_modes(&params, false),
            (Some(b'?' | b'>' | b'<' | b'='), None, b'u' | b'n' | b'c' | b'q') => {
                self.dispatch_private_query_or_kitty(prefix, p0, p1, final_byte);
            }
            (None, None, b'c' | b'n' | b't') => self.dispatch_device_query(p0, final_byte),
            (None, None, _) => self.dispatch_standard_csi(&params, final_byte),
            _ => {}
        }
    }

    fn set_dec_modes(&mut self, params: &[u16], enable: bool) {
        for &m in params {
            if matches!(m, 47 | 1047 | 1049) {
                if enable {
                    self.enter_alt_screen(m);
                } else {
                    self.leave_alt_screen(m);
                }
            } else if m == 6 {
                self.active_mut().cursor.origin_mode = enable;
                self.move_cursor_home();
            } else if enable {
                self.dec_modes.insert(m);
            } else {
                self.dec_modes.remove(&m);
            }
        }
    }

    fn dispatch_private_query_or_kitty(
        &mut self,
        prefix: Option<u8>,
        p0: u16,
        p1: u16,
        final_byte: u8,
    ) {
        match (prefix, final_byte) {
            (Some(b'?'), b'n') if p0 == 996 => {
                let scheme = if self.dark() { 1 } else { 2 };
                let _ = write_fmt_bytes(&mut self.responses, format_args!("\x1b[?997;{scheme}n"));
            }
            (Some(b'?'), b'u') => {
                let flags = self.kitty_flags();
                let _ = write_fmt_bytes(&mut self.responses, format_args!("\x1b[?{flags}u"));
            }
            (Some(b'>'), b'u') => {
                let flags = u8::try_from(p0 & 0x1f).unwrap_or(0);
                let stack = &mut self.active_mut().kitty_stack;
                if stack.len() >= 16 {
                    stack.remove(0);
                }
                stack.push(flags);
            }
            (Some(b'<'), b'u') => {
                let n = usize::from(p0.max(1));
                let stack = &mut self.active_mut().kitty_stack;
                stack.truncate(stack.len().saturating_sub(n));
            }
            (Some(b'='), b'u') => {
                let flags = u8::try_from(p0 & 0x1f).unwrap_or(0);
                let mode = p1.max(1);
                let stack = &mut self.active_mut().kitty_stack;
                let cur = stack.pop().unwrap_or(0);
                let next = match mode {
                    2 => cur | flags,
                    3 => cur & !flags,
                    _ => flags,
                };
                if next != 0 {
                    stack.push(next);
                }
            }
            (Some(b'>'), b'c') if p0 == 0 => {
                self.responses.extend_from_slice(b"\x1b[>1;10;0c");
            }
            (Some(b'>'), b'q') if p0 == 0 => {
                let ver = self.xtversion.clone();
                let _ = write_fmt_bytes(&mut self.responses, format_args!("\x1bP>|{ver}\x1b\\"));
            }
            _ => {}
        }
    }

    fn dispatch_device_query(&mut self, p0: u16, final_byte: u8) {
        match (final_byte, p0) {
            (b'c', 0) => self.responses.extend_from_slice(b"\x1b[?62;22c"),
            (b'n', 5) => self.responses.extend_from_slice(b"\x1b[0n"),
            (b'n', 6) => {
                let cur = self.active().cursor;
                let _ = write_fmt_bytes(
                    &mut self.responses,
                    format_args!("\x1b[{};{}R", cur.row + 1, cur.col + 1),
                );
            }
            (b't', 16) => {
                let (cw, ch) = self.cell;
                let _ = write_fmt_bytes(&mut self.responses, format_args!("\x1b[6;{ch};{cw}t"));
            }
            (b't', 18) => {
                let _ = write_fmt_bytes(
                    &mut self.responses,
                    format_args!("\x1b[8;{};{}t", self.rows, self.cols),
                );
            }
            _ => {}
        }
    }

    fn move_cursor_home(&mut self) {
        let buf = self.active_mut();
        buf.cursor.wrap_next = false;
        buf.cursor.col = 0;
        buf.cursor.row = if buf.cursor.origin_mode {
            buf.scroll_top
        } else {
            0
        };
    }

    fn dispatch_standard_csi(&mut self, params: &[u16], final_byte: u8) {
        let alt = self.alt_active;
        let p0 = params.first().copied().unwrap_or(0);
        let p1 = params.get(1).copied().unwrap_or(0);
        if matches!(
            final_byte,
            b'J' | b'K' | b'L' | b'M' | b'S' | b'T' | b'P' | b'@' | b'X'
        ) {
            self.dispatch_edit_csi(p0, final_byte, alt);
            return;
        }
        if matches!(
            final_byte,
            b'A' | b'B'
                | b'e'
                | b'C'
                | b'a'
                | b'D'
                | b'E'
                | b'F'
                | b'G'
                | b'`'
                | b'H'
                | b'f'
                | b'd'
        ) {
            dispatch_cursor_csi(self.active_mut(), p0, p1, final_byte);
            return;
        }
        let buf = self.active_mut();
        match final_byte {
            b'r' => {
                let top = if p0 == 0 { 0 } else { usize::from(p0 - 1) };
                let bottom = if p1 == 0 {
                    buf.rows - 1
                } else {
                    usize::from(p1 - 1).min(buf.rows - 1)
                };
                if top < bottom {
                    buf.scroll_top = top;
                    buf.scroll_bottom = bottom;
                } else {
                    buf.reset_margins();
                }
                self.move_cursor_home();
            }
            b's' => buf.saved_cursor = Some(buf.cursor),
            b'u' => {
                if let Some(saved) = buf.saved_cursor {
                    buf.cursor = saved;
                    buf.cursor.col = buf.cursor.col.min(buf.cols - 1);
                    buf.cursor.row = buf.cursor.row.min(buf.rows - 1);
                }
            }
            b'm' => apply_sgr(&mut buf.cursor.style, params),
            _ => {}
        }
    }

    fn dispatch_edit_csi(&mut self, p0: u16, final_byte: u8, alt: bool) {
        let buf = self.active_mut();
        let fill = Style {
            bg: buf.cursor.style.bg,
            ..Style::default()
        };
        match final_byte {
            b'J' => erase_display(buf, p0, fill),
            b'K' => erase_line(buf, p0, fill),
            b'L' => {
                let n = usize::from(p0.max(1));
                let r = buf.cursor.row;
                let bottom = buf.scroll_bottom;
                if r >= buf.scroll_top && r <= bottom {
                    buf.scroll_down_region(r, bottom, n);
                }
            }
            b'M' => {
                let n = usize::from(p0.max(1));
                let r = buf.cursor.row;
                let bottom = buf.scroll_bottom;
                if r >= buf.scroll_top && r <= bottom {
                    buf.scroll_up_region(r, bottom, n, false);
                }
            }
            b'S' => {
                let n = usize::from(p0.max(1));
                let (top, bottom) = (buf.scroll_top, buf.scroll_bottom);
                buf.scroll_up_region(top, bottom, n, !alt);
            }
            b'T' => {
                let n = usize::from(p0.max(1));
                let (top, bottom) = (buf.scroll_top, buf.scroll_bottom);
                buf.scroll_down_region(top, bottom, n);
            }
            b'P' => {
                let n = usize::from(p0.max(1)).min(buf.cols - buf.cursor.col);
                let row = &mut buf.lines[buf.cursor.row];
                let col = buf.cursor.col;
                row.cells.drain(col..col + n);
                row.cells.resize(buf.cols, Cell::blank(fill));
            }
            b'@' => {
                let n = usize::from(p0.max(1)).min(buf.cols - buf.cursor.col);
                let row = &mut buf.lines[buf.cursor.row];
                let col = buf.cursor.col;
                for _ in 0..n {
                    row.cells.insert(col, Cell::blank(fill));
                }
                row.cells.truncate(buf.cols);
            }
            b'X' => {
                let n = usize::from(p0.max(1));
                let row = &mut buf.lines[buf.cursor.row];
                let end = (buf.cursor.col + n).min(buf.cols);
                for cell in &mut row.cells[buf.cursor.col..end] {
                    *cell = Cell::blank(fill);
                }
            }
            _ => {}
        }
    }

    fn dispatch_osc(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let Some((code, body)) = text.split_once(';') else {
            return;
        };
        match code {
            "0" | "2" if self.title != body => {
                self.title = body.to_string();
                self.title_changed = true;
            }
            "10" if body == "?" => self.push_osc_color(10, self.fg),
            "11" if body == "?" => self.push_osc_color(11, self.bg),
            "12" if body == "?" => self.push_osc_color(12, self.cursor_color),
            "7501" => {
                if body == "?" {
                    self.responses.extend_from_slice(b"\x1b]7501;?1\x1b\\");
                } else if let Some(report) = parse_osc_7501(body) {
                    self.statuses.push(report);
                }
            }
            _ => {}
        }
    }

    fn push_osc_color(&mut self, code: u8, [r, g, b]: [u8; 3]) {
        let _ = write_fmt_bytes(
            &mut self.responses,
            format_args!("\x1b]{code};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x07"),
        );
    }
}

fn utf8_char_len(first: u8) -> Option<usize> {
    if first & 0xe0 == 0xc0 {
        Some(2)
    } else if first & 0xf0 == 0xe0 {
        Some(3)
    } else if first & 0xf8 == 0xf0 {
        Some(4)
    } else {
        None
    }
}

fn dispatch_cursor_csi(buf: &mut BufferState, p0: u16, p1: u16, final_byte: u8) {
    buf.cursor.wrap_next = false;
    match final_byte {
        b'A' => {
            let n = usize::from(p0.max(1));
            let floor = if buf.cursor.row >= buf.scroll_top {
                buf.scroll_top
            } else {
                0
            };
            buf.cursor.row = buf.cursor.row.saturating_sub(n).max(floor);
        }
        b'B' | b'e' => {
            let n = usize::from(p0.max(1));
            let ceil = if buf.cursor.row <= buf.scroll_bottom {
                buf.scroll_bottom
            } else {
                buf.rows - 1
            };
            buf.cursor.row = (buf.cursor.row + n).min(ceil);
        }
        b'C' | b'a' => {
            let n = usize::from(p0.max(1));
            buf.cursor.col = (buf.cursor.col + n).min(buf.cols - 1);
        }
        b'D' => {
            let n = usize::from(p0.max(1));
            buf.cursor.col = buf.cursor.col.saturating_sub(n);
        }
        b'E' => {
            let n = usize::from(p0.max(1));
            buf.cursor.col = 0;
            buf.cursor.row = (buf.cursor.row + n).min(buf.rows - 1);
        }
        b'F' => {
            let n = usize::from(p0.max(1));
            buf.cursor.col = 0;
            buf.cursor.row = buf.cursor.row.saturating_sub(n);
        }
        b'G' | b'`' => {
            let col = usize::from(p0.max(1)) - 1;
            buf.cursor.col = col.min(buf.cols - 1);
        }
        b'H' | b'f' => {
            let row = usize::from(p0.max(1)) - 1;
            let col = usize::from(p1.max(1)) - 1;
            let base = if buf.cursor.origin_mode {
                buf.scroll_top
            } else {
                0
            };
            let max_r = if buf.cursor.origin_mode {
                buf.scroll_bottom
            } else {
                buf.rows - 1
            };
            buf.cursor.row = (base + row).min(max_r);
            buf.cursor.col = col.min(buf.cols - 1);
        }
        b'd' => {
            let row = usize::from(p0.max(1)) - 1;
            buf.cursor.row = row.min(buf.rows - 1);
        }
        _ => {}
    }
}

fn erase_display(buf: &mut BufferState, mode: u16, fill: Style) {
    match mode {
        0 => {
            let r = buf.cursor.row;
            let c = buf.cursor.col;
            for cell in &mut buf.lines[r].cells[c..] {
                *cell = Cell::blank(fill);
            }
            for row in &mut buf.lines[r + 1..] {
                *row = Row::blank(buf.cols, fill);
            }
        }
        1 => {
            let r = buf.cursor.row;
            let c = buf.cursor.col;
            for row in &mut buf.lines[..r] {
                *row = Row::blank(buf.cols, fill);
            }
            for cell in &mut buf.lines[r].cells[..=c.min(buf.cols - 1)] {
                *cell = Cell::blank(fill);
            }
        }
        2 => {
            for row in &mut buf.lines {
                *row = Row::blank(buf.cols, fill);
            }
        }
        3 => {
            buf.scrollback.clear();
            buf.scrollback_bytes = 0;
        }
        _ => {}
    }
}

fn erase_line(buf: &mut BufferState, mode: u16, fill: Style) {
    let r = buf.cursor.row;
    let c = buf.cursor.col.min(buf.cols - 1);
    match mode {
        0 => {
            for cell in &mut buf.lines[r].cells[c..] {
                *cell = Cell::blank(fill);
            }
        }
        1 => {
            for cell in &mut buf.lines[r].cells[..=c] {
                *cell = Cell::blank(fill);
            }
        }
        2 => buf.lines[r] = Row::blank(buf.cols, fill),
        _ => {}
    }
}

fn apply_sgr(style: &mut Style, params: &[u16]) {
    if params.is_empty() {
        *style = Style::default();
        return;
    }
    let mut i = 0usize;
    while i < params.len() {
        match params[i] {
            0 => *style = Style::default(),
            1 => style.attrs |= ATTR_BOLD,
            2 => style.attrs |= ATTR_DIM,
            3 => style.attrs |= ATTR_ITALIC,
            4 => style.attrs |= ATTR_UNDERLINE,
            5 => style.attrs |= ATTR_BLINK,
            7 => style.attrs |= ATTR_INVERSE,
            8 => style.attrs |= ATTR_HIDDEN,
            9 => style.attrs |= ATTR_STRIKE,
            22 => style.attrs &= !(ATTR_BOLD | ATTR_DIM),
            23 => style.attrs &= !ATTR_ITALIC,
            24 => style.attrs &= !ATTR_UNDERLINE,
            25 => style.attrs &= !ATTR_BLINK,
            27 => style.attrs &= !ATTR_INVERSE,
            28 => style.attrs &= !ATTR_HIDDEN,
            29 => style.attrs &= !ATTR_STRIKE,
            30..=37 => style.fg = Color::Indexed(u8::try_from(params[i] - 30).unwrap_or(0)),
            39 => style.fg = Color::Default,
            40..=47 => style.bg = Color::Indexed(u8::try_from(params[i] - 40).unwrap_or(0)),
            49 => style.bg = Color::Default,
            90..=97 => style.fg = Color::Indexed(u8::try_from(params[i] - 90 + 8).unwrap_or(8)),
            100..=107 => style.bg = Color::Indexed(u8::try_from(params[i] - 100 + 8).unwrap_or(8)),
            38 | 48 => {
                let is_fg = params[i] == 38;
                let (color, consumed) = parse_extended_color(&params[i + 1..]);
                if let Some(c) = color {
                    if is_fg {
                        style.fg = c;
                    } else {
                        style.bg = c;
                    }
                }
                i += consumed;
            }
            _ => {}
        }
        i += 1;
    }
}

fn parse_extended_color(rest: &[u16]) -> (Option<Color>, usize) {
    match rest.first().copied() {
        Some(5) if rest.len() >= 2 => {
            let idx = u8::try_from(rest[1].min(255)).unwrap_or(0);
            (Some(Color::Indexed(idx)), 2)
        }
        Some(2) if rest.len() >= 4 => {
            let r = u8::try_from(rest[1].min(255)).unwrap_or(0);
            let g = u8::try_from(rest[2].min(255)).unwrap_or(0);
            let b = u8::try_from(rest[3].min(255)).unwrap_or(0);
            (Some(Color::Rgb(r, g, b)), 4)
        }
        _ => (None, 0),
    }
}

fn parse_osc_7501(body: &str) -> Option<StatusReport> {
    let mut state: Option<&'static str> = None;
    let mut kind: Option<&'static str> = None;
    let mut progress: Option<u8> = None;
    let mut id = String::new();
    let mut app = String::new();
    let mut title = String::new();
    let mut message = String::new();

    for field in body.split(':') {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        match k {
            "state" => {
                state = Some(match v {
                    "idle" => "idle",
                    "working" => "working",
                    "done" => "done",
                    "blocked" => "blocked",
                    "error" => "error",
                    "clear" => "clear",
                    _ => return None,
                });
            }
            "kind" => {
                kind = match v {
                    "permission" => Some("permission"),
                    "question" => Some("question"),
                    "auth" => Some("auth"),
                    _ => None,
                };
            }
            "progress" => {
                if let Ok(p) = v.parse::<u8>()
                    && p <= 100
                {
                    progress = Some(p);
                }
            }
            "id" => id = v.to_string(),
            "app" => app = v.to_string(),
            "title" => title = decode_base64_utf8(v),
            "msg" => message = decode_base64_utf8(v),
            _ => {}
        }
    }

    let state = state?;
    if state != "blocked" {
        kind = None;
    }
    if !matches!(state, "working" | "blocked") {
        progress = None;
    }
    Some(StatusReport {
        state,
        kind,
        progress,
        id,
        app,
        title,
        message,
    })
}

fn decode_base64_utf8(raw: &str) -> String {
    decode_standard_base64(raw.as_bytes()).map_or_else(
        || raw.to_string(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn decode_standard_base64(input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return Some(Vec::new());
    }
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let pad = match input {
        [.., b'=', b'='] => 2,
        [.., b'='] => 1,
        _ => 0,
    };
    let mut out = Vec::with_capacity((input.len() / 4) * 3 - pad);
    for (idx, chunk) in input.chunks_exact(4).enumerate() {
        let is_last = (idx + 1) * 4 == input.len();
        let mut vals = [0_u8; 4];
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                if !is_last || i < 4 - pad {
                    return None;
                }
                vals[i] = 0;
            } else {
                vals[i] = b64_val(b)?;
            }
        }
        let keep = if is_last { 3 - pad } else { 3 };
        out.push((vals[0] << 2) | (vals[1] >> 4));
        if keep >= 2 {
            out.push((vals[1] << 4) | (vals[2] >> 2));
        }
        if keep >= 3 {
            out.push((vals[2] << 6) | vals[3]);
        }
    }
    Some(out)
}

const fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
