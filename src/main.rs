use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
};

use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::Print,
    terminal::{self, ClearType},
};
use unicode_width::UnicodeWidthChar;

// ─── Help text ────────────────────────────────────────────────────────────────

const HELP: &[&str] = &[
    "─── Help (Ctrl+H to close) ──────────────────────────────────────",
    " Ctrl+S        Save               Ctrl+W / F2    Save As",
    " Ctrl+Q        Quit               Ctrl+Z              Undo",
    " Ctrl+A        Line beginning     Ctrl+E              Line end",
    " Arrow keys    Move cursor        Home / End          Line home / end",
    " PageUp/Down   Scroll page        Ctrl+H              Toggle this help",
];

const HELP_ROWS: usize = 6; // must equal HELP.len()

// ─── Prompt state ─────────────────────────────────────────────────────────────

enum Prompt {
    None,
    SaveAs(Vec<char>),
}

// ─── Undo snapshot ────────────────────────────────────────────────────────────

struct Snapshot {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

// ─── Editor state ────────────────────────────────────────────────────────────

struct Editor {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    top: usize,
    term_cols: u16,
    term_rows: u16,
    path: Option<PathBuf>,
    modified: bool,
    status: String,
    prompt: Prompt,
    undo_stack: Vec<Snapshot>,
    show_help: bool,
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
            status: String::from("Ctrl+S: Save  Ctrl+W/F2: Save As  Ctrl+H: Help  Ctrl+Q: Quit"),
            prompt: Prompt::None,
            undo_stack: Vec::new(),
            show_help: false,
        }
    }

    /// Rows available for text content (accounts for status bar, message bar, help panel).
    fn text_rows(&self) -> usize {
        let reserved = 2 + if self.show_help { HELP_ROWS } else { 0 };
        self.term_rows.saturating_sub(reserved as u16) as usize
    }

    // ── Undo ─────────────────────────────────────────────────────────────────

    fn push_undo(&mut self) {
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        // Cap history at 1000 entries to bound memory usage.
        if self.undo_stack.len() > 1000 {
            self.undo_stack.remove(0);
        }
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.lines = snap.lines;
            self.row = snap.row;
            self.col = snap.col;
            self.modified = true;
            self.status = "Undo".into();
            // Repair scroll offset if cursor is now above the viewport.
            if self.row < self.top {
                self.top = self.row;
            }
            let text_rows = self.text_rows();
            if self.row >= self.top + text_rows {
                self.top = self.row.saturating_sub(text_rows - 1);
            }
        } else {
            self.status = "Nothing to undo.".into();
        }
    }

    // ── File I/O ─────────────────────────────────────────────────────────────

    fn open(&mut self, path: PathBuf) {
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => {
                    self.lines = content.lines().map(|l| l.chars().collect()).collect();
                    if self.lines.is_empty() {
                        self.lines.push(vec![]);
                    }
                    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    self.status = format!("Opened: {}  |  Ctrl+H: Help", name);
                }
                Err(e) => {
                    self.status = format!("Error reading file: {}", e);
                }
            }
        } else {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.status = format!("New file: {}  |  Ctrl+H: Help", name);
        }
        self.path = Some(path);
    }

    fn save_to(&mut self, path: PathBuf) -> io::Result<()> {
        let content: String = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content)?;
        self.modified = false;
        self.status = format!("Saved: {}", path.display());
        self.path = Some(path);
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        match &self.path {
            Some(p) => {
                let p = p.clone();
                self.save_to(p)
            }
            None => {
                self.start_save_as();
                Ok(())
            }
        }
    }

    fn start_save_as(&mut self) {
        // Pre-fill with current filename if available.
        let prefill: Vec<char> = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().chars().collect())
            .unwrap_or_default();
        self.status = format!("Save as: {}", prefill.iter().collect::<String>());
        self.prompt = Prompt::SaveAs(prefill);
    }

    // ── Prompt handling ───────────────────────────────────────────────────────

    /// Returns true if the prompt consumed the key.
    fn handle_prompt_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> io::Result<bool> {
        if matches!(self.prompt, Prompt::None) {
            return Ok(false);
        }

        match code {
            KeyCode::Esc => {
                self.prompt = Prompt::None;
                self.status = "Cancelled.".into();
            }
            KeyCode::Enter => {
                if let Prompt::SaveAs(ref buf) = self.prompt {
                    let name: String = buf.iter().collect();
                    let path = PathBuf::from(&name);
                    self.prompt = Prompt::None;
                    if name.is_empty() {
                        self.status = "Cancelled – no filename given.".into();
                    } else {
                        self.save_to(path)?;
                    }
                }
            }
            KeyCode::Backspace => {
                if let Prompt::SaveAs(ref mut buf) = self.prompt {
                    buf.pop();
                    let s: String = buf.iter().collect();
                    self.status = format!("Save as: {}", s);
                }
            }
            KeyCode::Char(c)
                if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT =>
            {
                if let Prompt::SaveAs(ref mut buf) = self.prompt {
                    buf.push(c);
                    let s: String = buf.iter().collect();
                    self.status = format!("Save as: {}", s);
                }
            }
            _ => {}
        }
        Ok(true)
    }

    // ── Cursor helpers ────────────────────────────────────────────────────────

    fn display_col(&self, row: usize, col: usize) -> u16 {
        self.lines[row][..col]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>() as u16
    }

    // ── Editing operations ────────────────────────────────────────────────────

    fn insert_char(&mut self, ch: char) {
        self.push_undo();
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
        self.modified = true;
    }

    fn insert_newline(&mut self) {
        self.push_undo();
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
        if self.col > 0 || self.row > 0 {
            self.push_undo();
        }
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
            self.modified = true;
        } else if self.row > 0 {
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
        let can_delete =
            self.col < line_len || self.row + 1 < self.lines.len();
        if can_delete {
            self.push_undo();
        }
        if self.col < line_len {
            self.lines[self.row].remove(self.col);
            self.modified = true;
        } else if self.row + 1 < self.lines.len() {
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
        self.top = self.top.saturating_sub(text_rows);
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

        // ── Text area ─────────────────────────────────────────────────────────
        for screen_row in 0..text_rows {
            let doc_row = self.top + screen_row;
            queue!(
                stdout,
                cursor::MoveTo(0, screen_row as u16),
                terminal::Clear(ClearType::CurrentLine)
            )?;
            if doc_row < self.lines.len() {
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

        // ── Help panel (shown above status bar when active) ───────────────────
        if self.show_help {
            let help_start = text_rows as u16;
            for (i, line) in HELP.iter().enumerate() {
                queue!(
                    stdout,
                    cursor::MoveTo(0, help_start + i as u16),
                    terminal::Clear(ClearType::CurrentLine),
                    crossterm::style::SetAttribute(crossterm::style::Attribute::Dim),
                    Print(line),
                    crossterm::style::SetAttribute(crossterm::style::Attribute::Reset)
                )?;
            }
        }

        // ── Status bar ────────────────────────────────────────────────────────
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

        // ── Message bar / prompt ──────────────────────────────────────────────
        queue!(
            stdout,
            cursor::MoveTo(0, self.term_rows - 1),
            terminal::Clear(ClearType::CurrentLine),
            Print(&self.status)
        )?;

        // ── Final cursor position ─────────────────────────────────────────────
        match &self.prompt {
            Prompt::None => {
                let cursor_screen_row = (self.row - self.top) as u16;
                let cursor_screen_col = self.display_col(self.row, self.col);
                queue!(stdout, cursor::MoveTo(cursor_screen_col, cursor_screen_row))?;
            }
            Prompt::SaveAs(buf) => {
                let prefix_len = "Save as: ".len() as u16;
                let typed_width: u16 =
                    buf.iter().map(|c| c.width().unwrap_or(0) as u16).sum();
                queue!(
                    stdout,
                    cursor::MoveTo(prefix_len + typed_width, self.term_rows - 1)
                )?;
            }
        }

        queue!(stdout, cursor::Show)?;
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

    // Enable the Kitty keyboard enhancement protocol where supported so that
    // modifier combinations like Ctrl+Shift+S are reported unambiguously.
    let keyboard_enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }

    let (cols, rows) = terminal::size()?;
    let mut editor = Editor::new(cols, rows);

    if let Some(p) = path {
        editor.open(p);
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
                if editor.handle_prompt_key(code, modifiers)? {
                    // consumed by prompt
                } else {
                    match (code, modifiers) {
                        // ── Quit ─────────────────────────────────────────────
                        (KeyCode::Char('q'), KeyModifiers::CONTROL) => break,

                        // ── Save / Save As ────────────────────────────────────
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            editor.save()?;
                        }
                        // ── Save As ───────────────────────────────────────────
                        (KeyCode::Char('w'), KeyModifiers::CONTROL)
                        | (KeyCode::F(2), _) => {
                            editor.start_save_as();
                        }

                        // ── Undo ─────────────────────────────────────────────
                        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
                            editor.undo();
                        }

                        // ── Line home / end ───────────────────────────────────
                        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                            editor.move_home();
                        }
                        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                            editor.move_end();
                        }

                        // ── Help toggle ───────────────────────────────────────
                        (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                            editor.show_help = !editor.show_help;
                        }

                        // ── Navigation ────────────────────────────────────────
                        (KeyCode::Enter, _) => editor.insert_newline(),
                        (KeyCode::Backspace, _) => editor.backspace(),
                        (KeyCode::Delete, _) => editor.delete(),
                        (KeyCode::Up, _) => editor.move_up(),
                        (KeyCode::Down, _) => editor.move_down(),
                        (KeyCode::Left, _) => editor.move_left(),
                        (KeyCode::Right, _) => editor.move_right(),
                        (KeyCode::Home, _) => editor.move_home(),
                        (KeyCode::End, _) => editor.move_end(),
                        (KeyCode::PageUp, _) => editor.page_up(),
                        (KeyCode::PageDown, _) => editor.page_down(),

                        // ── Character input ───────────────────────────────────
                        (KeyCode::Char(c), KeyModifiers::NONE)
                        | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                            editor.insert_char(c);
                        }

                        _ => {}
                    }
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

    if keyboard_enhanced {
        execute!(stdout, PopKeyboardEnhancementFlags)?;
    }
    execute!(stdout, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    Ok(())
}
