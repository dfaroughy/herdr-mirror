// Cell grid + renderer for the pane wrapper.
//
// Frame wire format (verified against preview 2026-06-30): the frame ANSI is
// strictly per-cell — ESC[r;cH ESC[..m <char> — with a trailing cursor CUP and
// ?25h/l visibility. No scroll regions, no relative moves. So a cell grid plus
// this small parser is a complete decoder; no VT emulator needed.

use std::fmt::Write as _;
use std::rc::Rc;

#[derive(Clone, PartialEq)]
pub struct Cell {
    /// Rc: runs of cells share one SGR allocation
    pub sgr: Rc<str>,
    pub ch: char,
}

#[derive(Default)]
pub struct Grid {
    pub rows: Vec<Vec<Option<Cell>>>,
    pub width: usize,
    pub height: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    /// 0-based last row with non-blank content
    pub content_bottom: usize,
    /// reused per-frame decode buffer (frames arrive many times a second)
    scratch: Vec<char>,
}

impl Grid {
    pub fn new() -> Grid {
        Grid { cursor_visible: true, ..Default::default() }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.clear();
    }

    pub fn clear(&mut self) {
        self.rows = vec![vec![None; self.width]; self.height];
        self.content_bottom = 0;
    }

    pub fn apply(&mut self, ansi: &str) {
        let mut chars = std::mem::take(&mut self.scratch);
        chars.clear();
        chars.extend(ansi.chars());
        let mut row = 0usize;
        let mut col = 0usize;
        let mut sgr: Rc<str> = Rc::from("");
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] == '\x1b' {
                if let Some((params, final_ch, len)) = parse_csi(&chars[i..]) {
                    match final_ch {
                        'H' => {
                            let mut it = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1).max(1));
                            row = it.next().unwrap_or(1) - 1;
                            col = it.next().unwrap_or(1) - 1;
                        }
                        'm' => {
                            sgr = Rc::from(chars[i..i + len].iter().collect::<String>());
                        }
                        'J' => self.clear(),
                        'h' | 'l' if params == "?25" => self.cursor_visible = final_ch == 'h',
                        _ => {}
                    }
                    i += len;
                    continue;
                }
                if let Some(len) = parse_osc(&chars[i..]) {
                    i += len;
                    continue;
                }
                i += 2; // two-byte escape (charset selection etc.)
                continue;
            }
            let ch = chars[i];
            if ch >= ' ' || ch == '\t' {
                if row < self.height && col < self.width {
                    let ch = if ch == '\t' { ' ' } else { ch };
                    self.rows[row][col] = Some(Cell { sgr: sgr.clone(), ch });
                }
                col += 1;
            }
            i += 1;
        }
        self.scratch = chars;
        // the scan position after the last CUP is the cursor: the frame ends
        // with an explicit cursor CUP followed only by visibility toggles
        self.cursor_row = row;
        self.cursor_col = col;
        // recompute (not just grow): a delta frame can erase content with
        // spaces, and a stale bottom would anchor the window onto blank rows
        self.content_bottom = self
            .rows
            .iter()
            .rposition(|cells| cells.iter().any(|c| c.as_ref().is_some_and(|c| c.ch != ' ')))
            .unwrap_or(0);
    }

    pub fn text_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|cells| {
                cells
                    .iter()
                    .map(|c| c.as_ref().map(|c| c.ch).unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// First grid row shown when painting into an out_rows-tall window
    /// (bottom-anchored). The mouse→grid coordinate map must use the same
    /// formula as the renderer or highlights land one row off.
    pub fn window_offset(&self, out_rows: usize) -> usize {
        let bottom = self.content_bottom.max(self.cursor_row);
        (bottom + 1).saturating_sub(out_rows)
    }

    /// Text of the inclusive linear range start..=end ((row, col), start ≤ end).
    /// Rows that are wrapped continuations — the grid row runs non-blank into
    /// its last column — join to the next row without a newline, so copied
    /// prose comes out as logical lines, not screen rows.
    pub fn extract_text(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let mut out = String::new();
        for r in start.0..=end.0.min(self.height.saturating_sub(1)) {
            let cells = &self.rows[r];
            let from = if r == start.0 { start.1 } else { 0 };
            let to = if r == end.0 { (end.1 + 1).min(self.width) } else { self.width };
            let row_text: String = (from..to)
                .map(|c| cells.get(c).and_then(|c| c.as_ref()).map(|c| c.ch).unwrap_or(' '))
                .collect();
            let last = r == end.0.min(self.height.saturating_sub(1));
            let wrapped = !last
                && self
                    .rows[r]
                    .last()
                    .and_then(|c| c.as_ref())
                    .is_some_and(|c| c.ch != ' ');
            if wrapped {
                out.push_str(&row_text);
            } else {
                out.push_str(row_text.trim_end());
                if !last {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Is grid position p inside the inclusive linear (row-major) range s..=e?
pub fn in_selection(p: (usize, usize), s: (usize, usize), e: (usize, usize)) -> bool {
    p >= s && p <= e
}

/// CSI: ESC [ <params: 0-9;:?> <final: alpha>. Returns (params, final, char len).
fn parse_csi(chars: &[char]) -> Option<(String, char, usize)> {
    if chars.len() < 3 || chars[0] != '\x1b' || chars[1] != '[' {
        return None;
    }
    let mut params = String::new();
    for (idx, &c) in chars.iter().enumerate().skip(2).take(62) {
        if c.is_ascii_digit() || c == ';' || c == ':' || c == '?' {
            params.push(c);
        } else if c.is_ascii_alphabetic() {
            return Some((params, c, idx + 1));
        } else {
            return None;
        }
    }
    None
}

/// OSC: ESC ] … (BEL | ESC \). Returns char len.
fn parse_osc(chars: &[char]) -> Option<usize> {
    if chars.len() < 2 || chars[0] != '\x1b' || chars[1] != ']' {
        return None;
    }
    let mut i = 2;
    while i < chars.len() {
        match chars[i] {
            '\x07' => return Some(i + 1),
            '\x1b' if chars.get(i + 1) == Some(&'\\') => return Some(i + 2),
            '\x1b' => return None,
            _ => i += 1,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// renderer: paints a window of the grid onto the local terminal

#[derive(Default)]
pub struct Renderer {
    last_rows: Vec<Option<String>>,
    status_text: String,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer::default()
    }

    pub fn invalidate(&mut self) {
        self.last_rows.clear();
    }

    pub fn status(&mut self, text: &str) {
        self.status_text = text.to_string();
        self.last_rows.pop(); // force bottom row repaint
    }

    /// Build the ANSI to paint the grid into an out_cols × out_rows terminal.
    /// Bottom-anchored window: agent TUIs live at the bottom of the screen.
    /// `sel`: normalized inclusive selection in GRID coords, reverse-video'd.
    pub fn paint(
        &mut self,
        grid: &Grid,
        out_cols: usize,
        out_rows: usize,
        sel: Option<((usize, usize), (usize, usize))>,
    ) -> String {
        let offset_r = grid.window_offset(out_rows);
        let mut out = String::from("\x1b[?2026h\x1b[?25l");
        // paint every local row (missing rows blank-fill), or the pane stays
        // blank before the first frame and the status row is unreachable
        let row_count = out_rows;
        if self.last_rows.len() < row_count {
            self.last_rows.resize(row_count, None);
        }
        for r in 0..row_count {
            let empty = Vec::new();
            let cells = grid.rows.get(r + offset_r).unwrap_or(&empty);
            let mut line = String::new();
            let mut prev_key: Option<(&str, bool)> = None;
            for c in 0..out_cols.min(grid.width) {
                let cell = cells.get(c).and_then(|c| c.as_ref());
                let sgr = cell.map(|c| &*c.sgr).unwrap_or("\x1b[0m");
                let selected = sel.is_some_and(|(s, e)| in_selection((r + offset_r, c), s, e));
                if prev_key != Some((sgr, selected)) {
                    line.push_str(if sgr.is_empty() { "\x1b[0m" } else { sgr });
                    if selected {
                        line.push_str("\x1b[7m");
                    }
                    prev_key = Some((sgr, selected));
                }
                line.push(cell.map(|c| c.ch).unwrap_or(' '));
            }
            let is_status_row = r == out_rows - 1 && !self.status_text.is_empty();
            let painted = if is_status_row {
                format!("\x1b[0;7m {} \x1b[0m\x1b[K", self.status_text)
            } else {
                format!("{line}\x1b[0m\x1b[K")
            };
            if self.last_rows.get(r).map(|p| p.as_deref()) != Some(Some(painted.as_str())) {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                out.push_str(&painted);
                self.last_rows[r] = Some(painted);
            }
        }
        let cr = grid.cursor_row as isize - offset_r as isize;
        if grid.cursor_visible && cr >= 0 && (cr as usize) < out_rows && self.status_text.is_empty() {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cr + 1, grid.cursor_col.min(out_cols.saturating_sub(1)) + 1);
        }
        out.push_str("\x1b[?2026l");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_per_cell_frame() {
        let mut g = Grid::new();
        g.resize(10, 4);
        g.apply("\x1b[1;1H\x1b[0mhi\x1b[3;2H\x1b[31mX\x1b[2;1H\x1b[?25h");
        assert_eq!(g.text_lines(), vec!["hi", "", " X", ""]);
        assert_eq!(g.content_bottom, 2);
        assert_eq!((g.cursor_row, g.cursor_col), (1, 0));
        assert!(g.cursor_visible);
        assert_eq!(&*g.rows[2][1].as_ref().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn clear_and_visibility() {
        let mut g = Grid::new();
        g.resize(4, 2);
        g.apply("\x1b[1;1Habcd\x1b[2;1Hwxyz");
        assert_eq!(g.content_bottom, 1);
        g.apply("\x1b[2J\x1b[?25l");
        assert_eq!(g.text_lines(), vec!["", ""]);
        assert!(!g.cursor_visible);
    }

    #[test]
    fn skips_osc_and_tabs() {
        let mut g = Grid::new();
        g.resize(8, 1);
        g.apply("\x1b]0;title\x07\x1b[1;1Ha\tb");
        assert_eq!(g.text_lines(), vec!["a b"]);
    }

    #[test]
    fn content_bottom_shrinks_when_delta_erases() {
        let mut g = Grid::new();
        g.resize(6, 8);
        g.apply("\x1b[1;1Htop\x1b[7;1Hbottom");
        assert_eq!(g.content_bottom, 6);
        // delta frame erases the bottom content with spaces
        g.apply("\x1b[7;1H      ");
        assert_eq!(g.content_bottom, 0);
    }

    #[test]
    fn extract_joins_wrapped_rows() {
        let mut g = Grid::new();
        g.resize(6, 4);
        // row0 runs into the last column (wrapped), rows 1-2 are short
        g.apply("\x1b[1;1Habcdef\x1b[2;1Hgh\x1b[3;1Hxy");
        // full selection: wrapped row joins, short row gets a newline
        assert_eq!(g.extract_text((0, 0), (2, 5)), "abcdefgh\nxy");
        // partial: mid-row start, mid-row end
        assert_eq!(g.extract_text((0, 2), (1, 0)), "cdefg");
        // single row slice
        assert_eq!(g.extract_text((1, 0), (1, 5)), "gh");
    }

    #[test]
    fn paint_reverses_selection() {
        let mut g = Grid::new();
        g.resize(4, 2);
        g.apply("\x1b[1;1Habcd\x1b[2;1Hwxyz");
        let mut r = Renderer::new();
        let out = r.paint(&g, 4, 2, Some(((0, 1), (0, 2))));
        assert!(out.contains("\x1b[7m"));
        // growing the selection repaints via the row diff alone
        let out2 = r.paint(&g, 4, 2, Some(((0, 1), (0, 3))));
        assert!(out2.contains("\x1b[7m"));
    }

    #[test]
    fn status_paints_on_empty_grid() {
        // before the first frame the grid is 0x0 — status must still render
        let g = Grid::new();
        let mut r = Renderer::new();
        r.status("reconnecting in 5s");
        let out = r.paint(&g, 80, 24, None);
        assert!(out.contains("reconnecting in 5s"));
    }

    #[test]
    fn renderer_bottom_anchors_and_status() {
        let mut g = Grid::new();
        g.resize(5, 10);
        g.apply("\x1b[10;1Hlast"); // content at the bottom row of a tall grid
        let mut r = Renderer::new();
        let out = r.paint(&g, 5, 3, None);
        // window shows rows 8..10 → "last" lands on the visible last row
        assert!(out.contains("last"));
        r.status("HELLO");
        let out2 = r.paint(&g, 5, 3, None);
        assert!(out2.contains("HELLO"));
        // unchanged rows are not repainted
        let out3 = r.paint(&g, 5, 3, None);
        assert!(!out3.contains("last"));
    }
}
