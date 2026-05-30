use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, ClearType},
};
use unicode_width::UnicodeWidthChar;

// ─── Editor state ────────────────────────────────────────────────────────────

struct Editor {
    /// Each line as a Vec<char> so indexing/removal is O(n) but correct.
    lines: Vec<Vec<char>>,
    /// Logical cursor: row index into `lines`.
    row: usize,
    /// Logical cursor: column index (character index, not byte/display).
    col: usize,
    /// First visible row (vertical scroll offset).
    top: usize,
    /// Terminal dimensions (cols, rows).
    term_cols: u16,
    term_rows: u16,
    /// File path (None if new / unnamed).
    path: Option<PathBuf>,
    /// Dirty flag.
    modified: bool,
    /// One-line status message shown at the bottom.
    status: String,
}

impl Editor {
    fn new(term_cols: u16, term_rows: u16) -> Self {
        Self {
            lines: vec![vec![]],
            row: 0,
            col: 0,
            top: 0,
            term_cols,
            term_rows,
            path: None,
            modified: false,
            status: String::from("Ctrl+S: Save  Ctrl+Q: Quit"),
        }
    }

    /// Number of rows available for text (bottom two rows = status + info bar).
    fn text_rows(&self) -> usize {
        self.term_rows.saturating_sub(2) as usize
    }

    // ── File I/O ─────────────────────────────────────────────────────────────

    fn load(&mut self, path: PathBuf) -> io::Result<()> {
        let content = fs::read_to_string(&path)?;
        self.lines = content
            .lines()
            .map(|l| l.chars().collect())
            .collect();
        if self.lines.is_empty() {
            self.lines.push(vec![]);
        }
        self.path = Some(path);
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        let path = match &self.path {
            Some(p) => p.clone(),
            None => {
                self.status = "No file path – pass a filename on the command line.".into();
                return Ok(());
            }
        };
        let content: String = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content)?;
        self.modified = false;
        self.status = format!("Saved: {}", path.display());
        Ok(())
    }

    // ── Cursor helpers ────────────────────────────────────────────────────────

    /// Display width of the current line up to logical column `col`.
    fn display_col(&self, row: usize, col: usize) -> u16 {
        self.lines[row][..col]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>() as u16
    }

    // ── Editing operations ────────────────────────────────────────────────────

    fn insert_char(&mut self, ch: char) {
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
        self.modified = true;
    }

    fn insert_newline(&mut self) {
        let rest: Vec<char> = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.modified = true;
        if self.row >= self.top + self.text_rows() {
            self.top += 1;
        }
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
            self.modified = true;
        } else if self.row > 0 {
            // Join with previous line
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(current);
            self.modified = true;
            if self.top > self.row {
                self.top = self.row;
            }
        }
    }

    fn delete(&mut self) {
        let line_len = self.lines[self.row].len();
        if self.col < line_len {
            self.lines[self.row].remove(self.col);
            self.modified = true;
        } else if self.row + 1 < self.lines.len() {
            // Join with next line
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
            self.modified = true;
        }
    }

    // ── Cursor movement ───────────────────────────────────────────────────────

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
            if self.row < self.top {
                self.top = self.row;
            }
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
            if self.row >= self.top + self.text_rows() {
                self.top += 1;
            }
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
            if self.row < self.top {
                self.top = self.row;
            }
        }
    }

    fn move_right(&mut self) {
        let line_len = self.lines[self.row].len();
        if self.col < line_len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
            if self.row >= self.top + self.text_rows() {
                self.top += 1;
            }
        }
    }

    fn move_home(&mut self) {
        self.col = 0;
    }

    fn move_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    fn page_up(&mut self) {
        let text_rows = self.text_rows();
        if self.top >= text_rows {
            self.top -= text_rows;
        } else {
            self.top = 0;
        }
        self.row = self.top;
        self.col = self.col.min(self.lines[self.row].len());
    }

    fn page_down(&mut self) {
        let text_rows = self.text_rows();
        let max_top = self.lines.len().saturating_sub(text_rows);
        self.top = (self.top + text_rows).min(max_top);
        self.row = (self.top + text_rows - 1).min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len());
    }

    // ── Rendering (full redraw, ConPTY-safe) ─────────────────────────────────

    fn render(&self, stdout: &mut impl Write) -> io::Result<()> {
        queue!(stdout, cursor::Hide)?;

        let text_rows = self.text_rows();

        for screen_row in 0..text_rows {
            let doc_row = self.top + screen_row;

            // Move to row start and clear the ENTIRE line.
            // We never use \b or partial-erase sequences — always full-line clear
            // from column 0. This is the core ConPTY safety measure.
            queue!(
                stdout,
                cursor::MoveTo(0, screen_row as u16),
                terminal::Clear(ClearType::CurrentLine)
            )?;

            if doc_row < self.lines.len() {
                // Render character by character, tracking display width, and
                // truncate at terminal width to prevent wrapping artefacts.
                let mut display_x: u16 = 0;
                for ch in &self.lines[doc_row] {
                    let w = ch.width().unwrap_or(0) as u16;
                    if display_x + w > self.term_cols {
                        break;
                    }
                    queue!(stdout, Print(ch))?;
                    display_x += w;
                }
            } else {
                queue!(stdout, Print("~"))?;
            }
        }

        // ── Status bar (second-to-last row) ──────────────────────────────────
        let status_row = self.term_rows - 2;
        let file_name = self
            .path
            .as_ref()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .unwrap_or_else(|| "[New File]".into());
        let dirty = if self.modified { " [+]" } else { "" };
        let pos = format!(" {}:{} ", self.row + 1, self.col + 1);
        let left = format!(" {}{}", file_name, dirty);
        let pad = (self.term_cols as usize).saturating_sub(left.len() + pos.len());
        let status_line = format!("{}{:>pad$}{}", left, pos, "", pad = pad);

        queue!(
            stdout,
            cursor::MoveTo(0, status_row),
            terminal::Clear(ClearType::CurrentLine),
            crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
            Print(format!("{:width$}", status_line, width = self.term_cols as usize)),
            crossterm::style::SetAttribute(crossterm::style::Attribute::Reset)
        )?;

        // ── Message bar (last row) ────────────────────────────────────────────
        queue!(
            stdout,
            cursor::MoveTo(0, self.term_rows - 1),
            terminal::Clear(ClearType::CurrentLine),
            Print(&self.status)
        )?;

        // ── Reposition cursor using absolute coordinates (never relative) ─────
        let cursor_screen_row = (self.row - self.top) as u16;
        let cursor_screen_col = self.display_col(self.row, self.col);
        queue!(
            stdout,
            cursor::MoveTo(cursor_screen_col, cursor_screen_row),
            cursor::Show
        )?;

        stdout.flush()?;
        Ok(())
    }
}

// ─── Main loop ────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).map(PathBuf::from);

    let mut stdout = io::stdout();

    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Show)?;

    let (cols, rows) = terminal::size()?;
    let mut editor = Editor::new(cols, rows);

    if let Some(p) = path {
        match editor.load(p) {
            Ok(()) => {
                let name = editor
                    .path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                editor.status = format!("Opened: {}  |  Ctrl+S: Save  Ctrl+Q: Quit", name);
            }
            Err(e) => {
                editor.status = format!("Error opening file: {}", e);
            }
        }
    }

    editor.render(&mut stdout)?;

    loop {
        let (cols, rows) = terminal::size()?;
        if cols != editor.term_cols || rows != editor.term_rows {
            editor.term_cols = cols;
            editor.term_rows = rows;
        }

        match event::read()? {
            Event::Key(KeyEvent { code, modifiers, kind, .. })
                if kind == KeyEventKind::Press || kind == KeyEventKind::Repeat =>
            {
                match (code, modifiers) {
                    (KeyCode::Char('q'), KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                        editor.save()?;
                    }
                    (KeyCode::Enter, _) => {
                        editor.insert_newline();
                    }
                    (KeyCode::Backspace, _) => {
                        editor.backspace();
                    }
                    (KeyCode::Delete, _) => {
                        editor.delete();
                    }
                    (KeyCode::Up, _) => editor.move_up(),
                    (KeyCode::Down, _) => editor.move_down(),
                    (KeyCode::Left, _) => editor.move_left(),
                    (KeyCode::Right, _) => editor.move_right(),
                    (KeyCode::Home, _) => editor.move_home(),
                    (KeyCode::End, _) => editor.move_end(),
                    (KeyCode::PageUp, _) => editor.page_up(),
                    (KeyCode::PageDown, _) => editor.page_down(),
                    (KeyCode::Char(c), KeyModifiers::NONE)
                    | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                        editor.insert_char(c);
                    }
                    _ => {}
                }
            }
            Event::Resize(cols, rows) => {
                editor.term_cols = cols;
                editor.term_rows = rows;
            }
            _ => {}
        }

        editor.render(&mut stdout)?;
    }

    execute!(stdout, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    Ok(())
}
