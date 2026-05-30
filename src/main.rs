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
    style::{Attribute, Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use unicode_width::UnicodeWidthChar;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Visual width of a TAB character (fixed, not variable tab-stops per line).
const TAB_WIDTH: usize = 4;

const HELP: &[&str] = &[
    "─── Help (Ctrl+H to close) ──────────────────────────────────────────",
    " Ctrl+S   Save          Ctrl+W / F2   Save As      Ctrl+Z   Undo",
    " Ctrl+C   Start/end copy selection    Ctrl+V        Paste",
    " Ctrl+X   Start/end cut  selection    Esc           Cancel selection",
    " Ctrl+A   Line home    Ctrl+E   Line end    Tab   Insert tab (→)",
    " Ctrl+Q   Quit (confirms if unsaved)  Ctrl+H   Toggle this help",
    " Arrow keys / Home / End / PageUp / PageDown   Move cursor",
];
const HELP_ROWS: usize = 7; // must equal HELP.len()

// ─── Prompt ───────────────────────────────────────────────────────────────────

enum Prompt {
    None,
    SaveAs { buf: Vec<char>, cur: usize },
    /// Unsaved-changes confirmation before quit.
    QuitConfirm,
}

// ─── Undo snapshot ────────────────────────────────────────────────────────────

struct Snapshot {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

// ─── Selection ────────────────────────────────────────────────────────────────

struct Selection {
    anchor_row: usize,
    anchor_col: usize,
    /// true = Ctrl+X (cut), false = Ctrl+C (copy)
    is_cut: bool,
}

// ─── Editor ───────────────────────────────────────────────────────────────────

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
    selection: Option<Selection>,
    /// Internal clipboard (also mirrors system clipboard when possible).
    clipboard: Vec<Vec<char>>,
    /// System clipboard handle — None if the OS clipboard is unavailable.
    sys_clipboard: Option<arboard::Clipboard>,
    /// When true the main loop should break after the next render.
    pending_quit: bool,
    /// When true, a successful save should set pending_quit.
    quit_after_save: bool,
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
            status: String::from(
                "Ctrl+S: Save  Ctrl+W/F2: Save As  Ctrl+H: Help  Ctrl+Q: Quit",
            ),
            prompt: Prompt::None,
            undo_stack: Vec::new(),
            show_help: false,
            selection: None,
            clipboard: Vec::new(),
            sys_clipboard: arboard::Clipboard::new().ok(),
            pending_quit: false,
            quit_after_save: false,
        }
    }

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
            if self.row < self.top {
                self.top = self.row;
            }
            let tr = self.text_rows();
            if self.row >= self.top + tr {
                self.top = self.row.saturating_sub(tr - 1);
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
                    self.lines =
                        content.lines().map(|l| l.chars().collect()).collect();
                    if self.lines.is_empty() {
                        self.lines.push(vec![]);
                    }
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    self.status = format!("Opened: {}  |  Ctrl+H: Help", name);
                }
                Err(e) => {
                    self.status = format!("Error reading file: {}", e);
                }
            }
        } else {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
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
        // If a quit was requested before save, complete the quit now.
        if self.quit_after_save {
            self.quit_after_save = false;
            self.pending_quit = true;
        }
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
        let prefill: Vec<char> = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().chars().collect())
            .unwrap_or_default();
        let cur = prefill.len();
        self.status = format!("Save as: {}", prefill.iter().collect::<String>());
        self.prompt = Prompt::SaveAs { buf: prefill, cur };
    }

    // ── Prompt handling ───────────────────────────────────────────────────────

    /// Returns true if the prompt consumed the key.
    fn handle_prompt_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> io::Result<bool> {
        match &self.prompt {
            Prompt::None => return Ok(false),
            Prompt::SaveAs { .. } | Prompt::QuitConfirm => {}
        }

        match &self.prompt {
            // ── QuitConfirm ───────────────────────────────────────────────────
            Prompt::QuitConfirm => {
                match code {
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        self.prompt = Prompt::None;
                        match self.path.clone() {
                            Some(p) => {
                                self.save_to(p)?;
                                self.pending_quit = true;
                            }
                            None => {
                                // Need filename first; quit after save completes.
                                self.quit_after_save = true;
                                self.start_save_as();
                            }
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.prompt = Prompt::None;
                        self.pending_quit = true;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
                        self.prompt = Prompt::None;
                        self.status = "Quit cancelled.".into();
                    }
                    _ => {} // ignore other keys while confirming
                }
            }

            // ── SaveAs ────────────────────────────────────────────────────────
            Prompt::SaveAs { .. } => {
                match code {
                    KeyCode::Esc => {
                        self.prompt = Prompt::None;
                        self.quit_after_save = false;
                        self.status = "Cancelled.".into();
                    }
                    KeyCode::Enter => {
                        if let Prompt::SaveAs { ref buf, .. } = self.prompt {
                            let name: String = buf.iter().collect();
                            let path = PathBuf::from(&name);
                            self.prompt = Prompt::None;
                            if name.is_empty() {
                                self.quit_after_save = false;
                                self.status = "Cancelled – no filename given.".into();
                            } else {
                                self.save_to(path)?;
                            }
                        }
                    }
                    KeyCode::Left => {
                        if let Prompt::SaveAs { ref mut cur, .. } = self.prompt {
                            *cur = cur.saturating_sub(1);
                        }
                    }
                    KeyCode::Right => {
                        if let Prompt::SaveAs { ref buf, ref mut cur } = self.prompt {
                            if *cur < buf.len() {
                                *cur += 1;
                            }
                        }
                    }
                    KeyCode::Home => {
                        if let Prompt::SaveAs { ref mut cur, .. } = self.prompt {
                            *cur = 0;
                        }
                    }
                    KeyCode::End => {
                        if let Prompt::SaveAs { ref buf, ref mut cur } = self.prompt {
                            *cur = buf.len();
                        }
                    }
                    KeyCode::Backspace => {
                        if let Prompt::SaveAs { ref mut buf, ref mut cur } = self.prompt {
                            if *cur > 0 {
                                *cur -= 1;
                                buf.remove(*cur);
                                let s: String = buf.iter().collect();
                                self.status = format!("Save as: {}", s);
                            }
                        }
                    }
                    KeyCode::Delete => {
                        if let Prompt::SaveAs { ref mut buf, ref cur } = self.prompt {
                            if *cur < buf.len() {
                                buf.remove(*cur);
                                let s: String = buf.iter().collect();
                                self.status = format!("Save as: {}", s);
                            }
                        }
                    }
                    KeyCode::Char(c)
                        if modifiers == KeyModifiers::NONE
                            || modifiers == KeyModifiers::SHIFT =>
                    {
                        if let Prompt::SaveAs { ref mut buf, ref mut cur } = self.prompt {
                            buf.insert(*cur, c);
                            *cur += 1;
                            let s: String = buf.iter().collect();
                            self.status = format!("Save as: {}", s);
                        }
                    }
                    _ => {}
                }
            }

            Prompt::None => unreachable!(),
        }

        Ok(true)
    }

    // ── Selection / clipboard ─────────────────────────────────────────────────

    /// Normalized (start, end) as (row, col) tuples; end is exclusive.
    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let sel = self.selection.as_ref()?;
        let anchor = (sel.anchor_row, sel.anchor_col);
        let cursor = (self.row, self.col);
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    fn in_selection(&self, row: usize, col: usize) -> bool {
        let Some(((sr, sc), (er, ec))) = self.selection_range() else {
            return false;
        };
        (row, col) >= (sr, sc) && (row, col) < (er, ec)
    }

    fn collect_selected(&self) -> Vec<Vec<char>> {
        let Some(((sr, sc), (er, ec))) = self.selection_range() else {
            return vec![];
        };
        if sr == er {
            return vec![self.lines[sr][sc..ec].to_vec()];
        }
        let mut out = vec![self.lines[sr][sc..].to_vec()];
        for r in (sr + 1)..er {
            out.push(self.lines[r].clone());
        }
        out.push(self.lines[er][..ec].to_vec());
        out
    }

    fn delete_selected(&mut self) {
        let Some(((sr, sc), (er, ec))) = self.selection_range() else {
            return;
        };
        self.push_undo();
        if sr == er {
            self.lines[sr].drain(sc..ec);
        } else {
            let suffix: Vec<char> = self.lines[er][ec..].to_vec();
            self.lines.drain((sr + 1)..=er);
            self.lines[sr].truncate(sc);
            self.lines[sr].extend(suffix);
        }
        self.row = sr;
        self.col = sc;
        self.selection = None;
        self.modified = true;
        if self.top > self.row {
            self.top = self.row;
        }
    }

    /// Convert internal clipboard to a plain string for the system clipboard.
    fn clipboard_to_string(&self) -> String {
        self.clipboard
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Write the current internal clipboard to the system clipboard.
    fn sync_to_sys_clipboard(&mut self) {
        let text = self.clipboard_to_string();
        if let Some(ref mut cb) = self.sys_clipboard {
            let _ = cb.set_text(text);
        }
    }

    /// Paste at cursor from system clipboard (if available), else internal.
    fn paste(&mut self) {
        // Read system clipboard; update internal clipboard if successful.
        let sys_text = self
            .sys_clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok());

        if let Some(text) = sys_text {
            self.clipboard = text
                .lines()
                .map(|l| l.chars().collect())
                .collect();
            if self.clipboard.is_empty() {
                self.clipboard.push(vec![]);
            }
        }

        if self.clipboard.is_empty() {
            return;
        }
        self.push_undo();

        if self.clipboard.len() == 1 {
            let chars = self.clipboard[0].clone();
            for ch in &chars {
                self.lines[self.row].insert(self.col, *ch);
                self.col += 1;
            }
        } else {
            let suffix: Vec<char> = self.lines[self.row].split_off(self.col);
            self.lines[self.row].extend(self.clipboard[0].iter());
            let last_idx = self.clipboard.len() - 1;
            for i in 1..last_idx {
                self.lines.insert(self.row + i, self.clipboard[i].clone());
            }
            let mut last_line = self.clipboard[last_idx].clone();
            let new_col = last_line.len();
            last_line.extend(suffix);
            self.lines.insert(self.row + last_idx, last_line);
            self.row += last_idx;
            self.col = new_col;
        }

        self.modified = true;
        let tr = self.text_rows();
        if self.row >= self.top + tr {
            self.top = self.row.saturating_sub(tr - 1);
        }
    }

    // ── Cursor / display helpers ──────────────────────────────────────────────

    /// Display column of logical cursor position, handling TAB expansion.
    fn display_col(&self, row: usize, col: usize) -> u16 {
        let mut x: usize = 0;
        for ch in &self.lines[row][..col] {
            if *ch == '\t' {
                x = (x / TAB_WIDTH + 1) * TAB_WIDTH;
            } else {
                x += ch.width().unwrap_or(0);
            }
        }
        x as u16
    }

    // ── Editing operations ────────────────────────────────────────────────────

    fn insert_char(&mut self, ch: char) {
        self.selection = None;
        self.push_undo();
        self.lines[self.row].insert(self.col, ch);
        self.col += 1;
        self.modified = true;
    }

    fn insert_tab(&mut self) {
        self.selection = None;
        self.push_undo();
        self.lines[self.row].insert(self.col, '\t');
        self.col += 1;
        self.modified = true;
    }

    fn insert_newline(&mut self) {
        self.selection = None;
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
        self.selection = None;
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
        self.selection = None;
        let line_len = self.lines[self.row].len();
        let can_delete = self.col < line_len || self.row + 1 < self.lines.len();
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
        let tr = self.text_rows();
        self.top = self.top.saturating_sub(tr);
        self.row = self.top;
        self.col = self.col.min(self.lines[self.row].len());
    }

    fn page_down(&mut self) {
        let tr = self.text_rows();
        let max_top = self.lines.len().saturating_sub(tr);
        self.top = (self.top + tr).min(max_top);
        self.row = (self.top + tr - 1).min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len());
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

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
                let mut display_x: usize = 0;
                // Track current render attribute state to minimise escape spam.
                // State: (is_selected, is_tab)
                let mut cur_attr: (bool, bool) = (false, false);

                for (char_idx, ch) in self.lines[doc_row].iter().enumerate() {
                    let is_tab = *ch == '\t';
                    let w = if is_tab {
                        TAB_WIDTH - (display_x % TAB_WIDTH)
                    } else {
                        ch.width().unwrap_or(0)
                    };

                    if display_x + w > self.term_cols as usize {
                        break;
                    }

                    let sel = self.in_selection(doc_row, char_idx);
                    let new_attr = (sel, is_tab);

                    if new_attr != cur_attr {
                        // Reset all attributes/colors, then re-apply as needed.
                        queue!(stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                        if sel {
                            queue!(stdout, Print(crossterm::style::SetAttribute(Attribute::Reverse)))?;
                        }
                        if is_tab {
                            // DarkGrey is reliably rendered as a dimmer color in
                            // Windows Terminal and most other modern terminals.
                            queue!(stdout, SetForegroundColor(Color::DarkGrey))?;
                        }
                        cur_attr = new_attr;
                    }

                    if is_tab {
                        // '>' is ASCII (always 1 column), safe on all terminals.
                        // U+2192 → is East-Asian-Ambiguous and renders as 2 cols
                        // in CJK terminals, which would break cursor alignment.
                        queue!(stdout, Print('>'))?;
                        for _ in 1..w {
                            queue!(stdout, Print(' '))?;
                        }
                    } else {
                        queue!(stdout, Print(ch))?;
                    }

                    display_x += w;
                }

                // Always reset color/attributes at end of line.
                if cur_attr != (false, false) {
                    queue!(stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                }
            } else {
                queue!(stdout, Print('~'))?;
            }
        }

        // ── Help panel ────────────────────────────────────────────────────────
        if self.show_help {
            let help_start = text_rows as u16;
            for (i, line) in HELP.iter().enumerate() {
                queue!(
                    stdout,
                    cursor::MoveTo(0, help_start + i as u16),
                    terminal::Clear(ClearType::CurrentLine),
                    Print(crossterm::style::SetAttribute(Attribute::Dim)),
                    Print(line),
                    Print(crossterm::style::SetAttribute(Attribute::Reset))
                )?;
            }
        }

        // ── Status bar ────────────────────────────────────────────────────────
        let status_row = self.term_rows - 2;
        let file_name = self
            .path
            .as_ref()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
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
            Print(crossterm::style::SetAttribute(Attribute::Reverse)),
            Print(format!("{:width$}", status_line, width = self.term_cols as usize)),
            Print(crossterm::style::SetAttribute(Attribute::Reset))
        )?;

        // ── Message bar / prompt ──────────────────────────────────────────────
        queue!(
            stdout,
            cursor::MoveTo(0, self.term_rows - 1),
            terminal::Clear(ClearType::CurrentLine),
            Print(&self.status)
        )?;

        // ── Cursor position ───────────────────────────────────────────────────
        match &self.prompt {
            Prompt::None | Prompt::QuitConfirm => {
                let cursor_screen_row = (self.row - self.top) as u16;
                let cursor_screen_col = self.display_col(self.row, self.col);
                queue!(stdout, cursor::MoveTo(cursor_screen_col, cursor_screen_row))?;
            }
            Prompt::SaveAs { buf, cur } => {
                let prefix_len = "Save as: ".len() as u16;
                let typed_width: u16 = buf[..*cur]
                    .iter()
                    .map(|c| c.width().unwrap_or(0) as u16)
                    .sum();
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

    'main: loop {
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
                    // Key was consumed by the active prompt.
                    if editor.pending_quit {
                        break 'main;
                    }
                } else {
                    match (code, modifiers) {
                        // ── Quit ─────────────────────────────────────────────
                        (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                            if editor.modified {
                                editor.prompt = Prompt::QuitConfirm;
                                editor.status =
                                    "Unsaved changes!  [S]ave and quit  [N]o  [C]ancel"
                                        .into();
                            } else {
                                break 'main;
                            }
                        }

                        // ── Save / Save As ────────────────────────────────────
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            editor.save()?;
                        }
                        (KeyCode::Char('w'), KeyModifiers::CONTROL)
                        | (KeyCode::F(2), _) => {
                            editor.start_save_as();
                        }

                        // ── Undo ─────────────────────────────────────────────
                        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
                            editor.undo();
                        }

                        // ── Copy (Ctrl+C) ─────────────────────────────────────
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            match editor.selection {
                                None => {
                                    editor.selection = Some(Selection {
                                        anchor_row: editor.row,
                                        anchor_col: editor.col,
                                        is_cut: false,
                                    });
                                    editor.status =
                                        "Copy: move cursor to selection end, then Ctrl+C again."
                                            .into();
                                }
                                Some(ref sel) if !sel.is_cut => {
                                    editor.clipboard = editor.collect_selected();
                                    editor.sync_to_sys_clipboard();
                                    editor.selection = None;
                                    editor.status = "Copied.".into();
                                }
                                Some(ref mut sel) => {
                                    sel.is_cut = false;
                                    editor.status =
                                        "Switched to copy mode. Press Ctrl+C again to copy."
                                            .into();
                                }
                            }
                        }

                        // ── Cut (Ctrl+X) ──────────────────────────────────────
                        (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                            match editor.selection {
                                None => {
                                    editor.selection = Some(Selection {
                                        anchor_row: editor.row,
                                        anchor_col: editor.col,
                                        is_cut: true,
                                    });
                                    editor.status =
                                        "Cut: move cursor to selection end, then Ctrl+X again."
                                            .into();
                                }
                                Some(ref sel) if sel.is_cut => {
                                    editor.clipboard = editor.collect_selected();
                                    editor.sync_to_sys_clipboard();
                                    editor.delete_selected();
                                    editor.status = "Cut.".into();
                                }
                                Some(ref mut sel) => {
                                    sel.is_cut = true;
                                    editor.status =
                                        "Switched to cut mode. Press Ctrl+X again to cut."
                                            .into();
                                }
                            }
                        }

                        // ── Paste (Ctrl+V) ────────────────────────────────────
                        (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                            editor.selection = None;
                            editor.paste();
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

                        // ── Cancel selection ──────────────────────────────────
                        (KeyCode::Esc, _) => {
                            if editor.selection.is_some() {
                                editor.selection = None;
                                editor.status = "Selection cancelled.".into();
                            }
                        }

                        // ── Navigation ────────────────────────────────────────
                        (KeyCode::Enter, _) => editor.insert_newline(),
                        (KeyCode::Tab, _) => editor.insert_tab(),
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
