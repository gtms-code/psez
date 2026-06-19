use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use chardetng::EncodingDetector;

use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use unicode_width::UnicodeWidthChar;

// ─── File encoding / line ending ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum FileEncoding {
    Utf8,
    Utf8Bom,
    ShiftJis,
    EucJp,
}

impl FileEncoding {
    fn label(self) -> &'static str {
        match self {
            FileEncoding::Utf8    => "UTF-8",
            FileEncoding::Utf8Bom => "UTF-8 BOM",
            FileEncoding::ShiftJis => "Shift-JIS",
            FileEncoding::EucJp   => "EUC-JP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    fn label(self) -> &'static str {
        match self {
            LineEnding::Lf   => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }

    fn separator(self) -> &'static str {
        match self {
            LineEnding::Lf   => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

/// Detect the character encoding of raw file bytes.
fn detect_encoding(bytes: &[u8]) -> FileEncoding {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        return FileEncoding::Utf8Bom;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return FileEncoding::Utf8;
    }
    let mut det = EncodingDetector::new();
    det.feed(bytes, true);
    let enc = det.guess(None, true);
    if enc == encoding_rs::SHIFT_JIS {
        FileEncoding::ShiftJis
    } else if enc == encoding_rs::EUC_JP {
        FileEncoding::EucJp
    } else {
        FileEncoding::Utf8
    }
}

/// Decode raw bytes to a String using the given encoding.
fn decode_bytes(bytes: &[u8], enc: FileEncoding) -> String {
    match enc {
        FileEncoding::Utf8    => String::from_utf8_lossy(bytes).into_owned(),
        FileEncoding::Utf8Bom => String::from_utf8_lossy(&bytes[3..]).into_owned(),
        FileEncoding::ShiftJis => {
            let (cow, _, _) = encoding_rs::SHIFT_JIS.decode(bytes);
            cow.into_owned()
        }
        FileEncoding::EucJp => {
            let (cow, _, _) = encoding_rs::EUC_JP.decode(bytes);
            cow.into_owned()
        }
    }
}

/// Encode a String to bytes using the given encoding.
fn encode_string(text: &str, enc: FileEncoding) -> Vec<u8> {
    match enc {
        FileEncoding::Utf8 => text.as_bytes().to_vec(),
        FileEncoding::Utf8Bom => {
            let mut out = b"\xEF\xBB\xBF".to_vec();
            out.extend_from_slice(text.as_bytes());
            out
        }
        FileEncoding::ShiftJis => {
            let (cow, _, _) = encoding_rs::SHIFT_JIS.encode(text);
            cow.into_owned()
        }
        FileEncoding::EucJp => {
            let (cow, _, _) = encoding_rs::EUC_JP.encode(text);
            cow.into_owned()
        }
    }
}

/// Detect the dominant line ending in a decoded string.
fn detect_line_ending(text: &str) -> LineEnding {
    if text.contains('\r') {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

// ─── Safety limits ────────────────────────────────────────────────────────────

const MAX_FILE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
const UNDO_MEM_CAP: usize = 64 * 1024 * 1024;  // 64 MB

fn snapshot_mem(snap: &Snapshot) -> usize {
    snap.lines.iter().map(|l| l.len() * 4).sum()
}

// ─── Constants ────────────────────────────────────────────────────────────────

const TAB_WIDTH: usize = 4;

const HELP: &[&str] = &[
    "─── Help (Ctrl+H to close) ──────────────────────────────────────────",
    " Ctrl+S: Save / Ctrl+W: Save As / Ctrl+Z: Undo / Ctrl+A: Select All",
    " Ctrl+C: Start & end copy selection / Ctrl+V: Paste",
    " Ctrl+X: Start & end cut  selection / Esc: Cancel selection",
    " Tab: Insert tab (>) / Ctrl+E: Change encoding (UTF8,SJIS,EUC)",
    " Ctrl+F: Search / Ctrl+R: Replace all / Ctrl+B: Toggle word wrap",
    " Ctrl+Q: Quit / Ctrl+H: Toggle this help",
    " Arrow keys / Home / End / PageUp / PageDown: Move cursor",
];
const HELP_ROWS: usize = 8; // must equal HELP.len()

// ─── ReplaceStep ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ReplaceStep {
    SearchInput,
    ReplaceInput,
}

// ─── EncStep ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum EncStep {
    ChooseEncoding,
    ChooseLineEnding(FileEncoding),
}

// ─── Prompt ───────────────────────────────────────────────────────────────────

enum Prompt {
    None,
    SaveAs { buf: Vec<char>, cur: usize },
    QuitConfirm,
    EncodingSelect(EncStep),
    SymlinkConfirm(PathBuf),
    Search { buf: Vec<char>, cur: usize },
    Replace {
        search_buf: Vec<char>,
        replace_buf: Vec<char>,
        search_cur: usize,
        replace_cur: usize,
        step: ReplaceStep,
    },
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
    is_cut: bool,
}

// ─── Editor ───────────────────────────────────────────────────────────────────

struct Editor {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    top: usize,
    left: usize,
    word_wrap: bool,
    term_cols: u16,
    term_rows: u16,
    path: Option<PathBuf>,
    modified: bool,
    status: String,
    prompt: Prompt,
    undo_stack: Vec<Snapshot>,
    undo_total_mem: usize,
    show_help: bool,
    selection: Option<Selection>,
    file_encoding: FileEncoding,
    line_ending: LineEnding,
    clipboard: Vec<Vec<char>>,
    sys_clipboard: Option<arboard::Clipboard>,
    pending_quit: bool,
    quit_after_save: bool,
    // Search / replace state
    search_query: Vec<char>,
    search_matches: Vec<(usize, usize)>, // (row, char_col) of each match start
    search_match_idx: usize,             // index of the currently highlighted match
}

impl Editor {
    fn new(term_cols: u16, term_rows: u16) -> Self {
        Self {
            lines: vec![vec![]],
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            word_wrap: false,
            term_cols,
            term_rows,
            path: None,
            modified: false,
            status: String::from("Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help"),
            prompt: Prompt::None,
            undo_stack: Vec::new(),
            undo_total_mem: 0,
            show_help: false,
            selection: None,
            file_encoding: FileEncoding::Utf8,
            line_ending: LineEnding::Lf,
            clipboard: Vec::new(),
            sys_clipboard: arboard::Clipboard::new().ok(),
            pending_quit: false,
            quit_after_save: false,
            search_query: Vec::new(),
            search_matches: Vec::new(),
            search_match_idx: 0,
        }
    }

    fn text_rows(&self) -> usize {
        let reserved = 2 + if self.show_help { HELP_ROWS } else { 0 };
        self.term_rows.saturating_sub(reserved as u16) as usize
    }

    // ── Undo ─────────────────────────────────────────────────────────────────

    fn push_undo(&mut self) {
        let snap = Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        };
        self.undo_total_mem += snapshot_mem(&snap);
        self.undo_stack.push(snap);
        while self.undo_total_mem > UNDO_MEM_CAP && self.undo_stack.len() > 1 {
            let evicted = self.undo_stack.remove(0);
            self.undo_total_mem =
                self.undo_total_mem.saturating_sub(snapshot_mem(&evicted));
        }
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.undo_total_mem =
                self.undo_total_mem.saturating_sub(snapshot_mem(&snap));
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
            if let Ok(meta) = fs::metadata(&path) {
                if meta.len() > MAX_FILE_BYTES {
                    self.status = format!(
                        "Error: file too large ({} MB). Limit is {} MB.",
                        meta.len() / 1024 / 1024,
                        MAX_FILE_BYTES / 1024 / 1024
                    );
                    return;
                }
            }
            match fs::read(&path) {
                Ok(bytes) => {
                    let enc = detect_encoding(&bytes);
                    let content = decode_bytes(&bytes, enc);
                    let le = detect_line_ending(&content);
                    self.lines =
                        content.lines().map(|l| l.chars().collect()).collect();
                    if self.lines.is_empty() {
                        self.lines.push(vec![]);
                    }
                    self.file_encoding = enc;
                    self.line_ending = le;
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    self.status = format!(
                        "Opened: {}  |  Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help",
                        name
                    );
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
            self.status = format!(
                "New file: {}  |  Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help",
                name
            );
        }
        self.path = Some(path);
    }

    fn save_to(&mut self, path: PathBuf) -> io::Result<()> {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                self.status = format!(
                    "\"{}\" is a symlink. Save to target anyway? [Y]es / [N]o",
                    path.display()
                );
                self.prompt = Prompt::SymlinkConfirm(path);
                return Ok(());
            }
            _ => {}
        }
        self.write_file(path)
    }

    fn write_file(&mut self, path: PathBuf) -> io::Result<()> {
        let content: String = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(self.line_ending.separator());
        let bytes = encode_string(&content, self.file_encoding);

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&bytes)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path).map_err(|e| e.error)?;

        self.modified = false;
        self.status = format!("Saved: {}", path.display());
        self.path = Some(path);
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

    // ── Search / replace helpers ──────────────────────────────────────────────

    /// Recompute all match positions for the current search_query.
    fn update_search(&mut self) {
        self.search_matches.clear();
        if self.search_query.is_empty() { return; }
        let qlen = self.search_query.len();
        for (r, line) in self.lines.iter().enumerate() {
            if line.len() < qlen { continue; }
            let limit = line.len() - qlen + 1;
            for c in 0..limit {
                if line[c..c + qlen] == self.search_query[..] {
                    self.search_matches.push((r, c));
                }
            }
        }
        if !self.search_matches.is_empty() {
            self.search_match_idx =
                self.search_match_idx.min(self.search_matches.len() - 1);
        } else {
            self.search_match_idx = 0;
        }
    }

    /// Index of the first match at or after (row, col); wraps to 0.
    fn find_nearest_match_from(&self, row: usize, col: usize) -> usize {
        for (i, &(r, c)) in self.search_matches.iter().enumerate() {
            if (r, c) >= (row, col) { return i; }
        }
        0
    }

    /// Scroll so the current match is visible and move the cursor there.
    fn jump_to_match(&mut self, idx: usize) {
        if self.search_matches.is_empty() { return; }
        let idx = idx % self.search_matches.len();
        self.search_match_idx = idx;
        let (r, c) = self.search_matches[idx];
        self.row = r;
        self.col = c;
        let tr = self.text_rows();
        if self.row < self.top { self.top = self.row; }
        if self.row >= self.top + tr {
            self.top = self.row.saturating_sub(tr / 2);
        }
    }

    /// Search highlight level for a character: 0=none, 1=match, 2=current match.
    fn search_attr_at(&self, row: usize, col: usize) -> u8 {
        if self.search_query.is_empty() { return 0; }
        let qlen = self.search_query.len();
        for (i, &(mr, mc)) in self.search_matches.iter().enumerate() {
            if mr == row && col >= mc && col < mc + qlen {
                return if i == self.search_match_idx { 2 } else { 1 };
            }
        }
        0
    }

    /// Select the entire document.
    fn select_all(&mut self) {
        self.selection = Some(Selection {
            anchor_row: 0,
            anchor_col: 0,
            is_cut: false,
        });
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.status = "Selected all.".into();
    }

    /// Replace every match of search_query with `replace`.
    fn replace_all(&mut self, replace: &[char]) {
        let count = self.search_matches.len();
        if count == 0 {
            self.status = "No matches to replace.".into();
            return;
        }
        self.push_undo();
        let qlen = self.search_query.len();
        // Process in reverse so earlier indices stay valid.
        for &(r, c) in self.search_matches.iter().rev() {
            if c + qlen <= self.lines[r].len() {
                self.lines[r].splice(c..c + qlen, replace.iter().cloned());
            }
        }
        self.modified = true;
        self.status = format!("Replaced {} occurrence(s).", count);
    }

    /// Helper: update search status string.
    fn search_status(&self) -> String {
        let q: String = self.search_query.iter().collect();
        if q.is_empty() {
            "Search: ".into()
        } else if self.search_matches.is_empty() {
            format!("Search: {}  (not found)", q)
        } else {
            format!(
                "Search: {}  [{}/{}]  Enter=next  Esc=close",
                q,
                self.search_match_idx + 1,
                self.search_matches.len()
            )
        }
    }

    // ── Prompt handling ───────────────────────────────────────────────────────

    fn handle_prompt_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> io::Result<bool> {
        match &self.prompt {
            Prompt::None => return Ok(false),
            Prompt::EncodingSelect(_) => return self.handle_enc_select_key(code),
            Prompt::SymlinkConfirm(_) => return self.handle_symlink_confirm_key(code),
            Prompt::Search { .. } => return self.handle_search_key(code, modifiers),
            Prompt::Replace { .. } => return self.handle_replace_key(code, modifiers),
            Prompt::SaveAs { .. } | Prompt::QuitConfirm => {}
        }

        match &self.prompt {
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
                                self.quit_after_save = true;
                                self.start_save_as();
                            }
                        }
                    }
                    KeyCode::Char('u') | KeyCode::Char('U') => {
                        self.prompt = Prompt::None;
                        self.pending_quit = true;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
                        self.prompt = Prompt::None;
                        self.status = "Quit cancelled.".into();
                    }
                    _ => {}
                }
            }

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
                            if *cur < buf.len() { *cur += 1; }
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

            Prompt::None
            | Prompt::EncodingSelect(_)
            | Prompt::SymlinkConfirm(_)
            | Prompt::Search { .. }
            | Prompt::Replace { .. } => unreachable!(),
        }

        Ok(true)
    }

    // ── Search prompt ─────────────────────────────────────────────────────────

    fn handle_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> io::Result<bool> {
        match code {
            KeyCode::Esc => {
                self.prompt = Prompt::None;
                self.search_query.clear();
                self.search_matches.clear();
                self.status = "Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help".into();
                return Ok(true);
            }
            KeyCode::Enter => {
                if !self.search_matches.is_empty() {
                    let next = (self.search_match_idx + 1) % self.search_matches.len();
                    self.jump_to_match(next);
                }
                self.status = self.search_status();
                return Ok(true);
            }
            _ => {}
        }

        // Text editing in the search buffer
        let changed = match code {
            KeyCode::Backspace => {
                let (changed, new_q) = if let Prompt::Search { ref mut buf, ref mut cur } = self.prompt {
                    if *cur > 0 {
                        *cur -= 1;
                        buf.remove(*cur);
                        (true, buf.clone())
                    } else { (false, buf.clone()) }
                } else { (false, vec![]) };
                if changed { self.search_query = new_q; }
                changed
            }
            KeyCode::Delete => {
                let (changed, new_q) = if let Prompt::Search { ref mut buf, ref cur } = self.prompt {
                    let c = *cur;
                    if c < buf.len() {
                        buf.remove(c);
                        (true, buf.clone())
                    } else { (false, buf.clone()) }
                } else { (false, vec![]) };
                if changed { self.search_query = new_q; }
                changed
            }
            KeyCode::Left => {
                if let Prompt::Search { ref mut cur, .. } = self.prompt {
                    *cur = cur.saturating_sub(1);
                }
                false
            }
            KeyCode::Right => {
                if let Prompt::Search { ref buf, ref mut cur } = self.prompt {
                    if *cur < buf.len() { *cur += 1; }
                }
                false
            }
            KeyCode::Home => {
                if let Prompt::Search { ref mut cur, .. } = self.prompt {
                    *cur = 0;
                }
                false
            }
            KeyCode::End => {
                if let Prompt::Search { ref buf, ref mut cur } = self.prompt {
                    *cur = buf.len();
                }
                false
            }
            KeyCode::Char(c)
                if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT =>
            {
                let (changed, new_q) = if let Prompt::Search { ref mut buf, ref mut cur } = self.prompt {
                    buf.insert(*cur, c);
                    *cur += 1;
                    (true, buf.clone())
                } else { (false, vec![]) };
                if changed { self.search_query = new_q; }
                changed
            }
            _ => false,
        };

        if changed {
            self.update_search();
            if !self.search_matches.is_empty() {
                let idx = self.find_nearest_match_from(self.row, self.col);
                self.jump_to_match(idx);
            }
        }
        self.status = self.search_status();
        Ok(true)
    }

    // ── Replace prompt ────────────────────────────────────────────────────────

    fn handle_replace_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> io::Result<bool> {
        let step = match &self.prompt {
            Prompt::Replace { step, .. } => *step as u8,
            _ => return Ok(false),
        };
        // step: 0 = SearchInput, 1 = ReplaceInput

        if step == 0 {
            // ── Step 1: editing the search query ─────────────────────────────
            match code {
                KeyCode::Esc => {
                    self.prompt = Prompt::None;
                    self.search_query.clear();
                    self.search_matches.clear();
                    self.status = "Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help".into();
                    return Ok(true);
                }
                KeyCode::Enter => {
                    // Move to replace-string step
                    if let Prompt::Replace { ref mut step, ref mut replace_cur, .. } = self.prompt {
                        *step = ReplaceStep::ReplaceInput;
                        *replace_cur = 0;
                    }
                    self.update_replace_status();
                    return Ok(true);
                }
                _ => {}
            }

            let changed = match code {
                KeyCode::Backspace => {
                    let (changed, new_q) = if let Prompt::Replace { ref mut search_buf, ref mut search_cur, .. } = self.prompt {
                        if *search_cur > 0 {
                            *search_cur -= 1;
                            search_buf.remove(*search_cur);
                            (true, search_buf.clone())
                        } else { (false, search_buf.clone()) }
                    } else { (false, vec![]) };
                    if changed { self.search_query = new_q; }
                    changed
                }
                KeyCode::Delete => {
                    let (changed, new_q) = if let Prompt::Replace { ref mut search_buf, ref search_cur, .. } = self.prompt {
                        let c = *search_cur;
                        if c < search_buf.len() {
                            search_buf.remove(c);
                            (true, search_buf.clone())
                        } else { (false, search_buf.clone()) }
                    } else { (false, vec![]) };
                    if changed { self.search_query = new_q; }
                    changed
                }
                KeyCode::Left => {
                    if let Prompt::Replace { ref mut search_cur, .. } = self.prompt {
                        *search_cur = search_cur.saturating_sub(1);
                    }
                    false
                }
                KeyCode::Right => {
                    if let Prompt::Replace { ref search_buf, ref mut search_cur, .. } = self.prompt {
                        if *search_cur < search_buf.len() { *search_cur += 1; }
                    }
                    false
                }
                KeyCode::Home => {
                    if let Prompt::Replace { ref mut search_cur, .. } = self.prompt { *search_cur = 0; }
                    false
                }
                KeyCode::End => {
                    if let Prompt::Replace { ref search_buf, ref mut search_cur, .. } = self.prompt {
                        *search_cur = search_buf.len();
                    }
                    false
                }
                KeyCode::Char(c)
                    if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT =>
                {
                    let (changed, new_q) = if let Prompt::Replace { ref mut search_buf, ref mut search_cur, .. } = self.prompt {
                        search_buf.insert(*search_cur, c);
                        *search_cur += 1;
                        (true, search_buf.clone())
                    } else { (false, vec![]) };
                    if changed { self.search_query = new_q; }
                    changed
                }
                _ => false,
            };

            if changed {
                self.update_search();
                if !self.search_matches.is_empty() {
                    let idx = self.find_nearest_match_from(self.row, self.col);
                    self.search_match_idx = idx;
                }
            }
            self.update_replace_status();
        } else {
            // ── Step 2: editing the replacement string ────────────────────────
            match code {
                KeyCode::Esc => {
                    self.prompt = Prompt::None;
                    self.search_query.clear();
                    self.search_matches.clear();
                    self.status = "Ctrl+S: Save / Ctrl+Q: Quit / Ctrl+H: Help".into();
                    return Ok(true);
                }
                KeyCode::Enter => {
                    let replace_buf = if let Prompt::Replace { ref replace_buf, .. } = self.prompt {
                        replace_buf.clone()
                    } else { vec![] };
                    self.replace_all(&replace_buf);
                    self.prompt = Prompt::None;
                    self.search_query.clear();
                    self.search_matches.clear();
                    return Ok(true);
                }
                _ => {}
            }

            match code {
                KeyCode::Backspace => {
                    if let Prompt::Replace { ref mut replace_buf, ref mut replace_cur, .. } = self.prompt {
                        if *replace_cur > 0 {
                            *replace_cur -= 1;
                            replace_buf.remove(*replace_cur);
                        }
                    }
                }
                KeyCode::Delete => {
                    if let Prompt::Replace { ref mut replace_buf, ref replace_cur, .. } = self.prompt {
                        let c = *replace_cur;
                        if c < replace_buf.len() { replace_buf.remove(c); }
                    }
                }
                KeyCode::Left => {
                    if let Prompt::Replace { ref mut replace_cur, .. } = self.prompt {
                        *replace_cur = replace_cur.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let Prompt::Replace { ref replace_buf, ref mut replace_cur, .. } = self.prompt {
                        if *replace_cur < replace_buf.len() { *replace_cur += 1; }
                    }
                }
                KeyCode::Home => {
                    if let Prompt::Replace { ref mut replace_cur, .. } = self.prompt { *replace_cur = 0; }
                }
                KeyCode::End => {
                    if let Prompt::Replace { ref replace_buf, ref mut replace_cur, .. } = self.prompt {
                        *replace_cur = replace_buf.len();
                    }
                }
                KeyCode::Char(c)
                    if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT =>
                {
                    if let Prompt::Replace { ref mut replace_buf, ref mut replace_cur, .. } = self.prompt {
                        replace_buf.insert(*replace_cur, c);
                        *replace_cur += 1;
                    }
                }
                _ => {}
            }
            self.update_replace_status();
        }

        Ok(true)
    }

    fn update_replace_status(&mut self) {
        let (q, r_str, count, step_num) = match &self.prompt {
            Prompt::Replace { search_buf, replace_buf, step, .. } => {
                let q: String = search_buf.iter().collect();
                let r: String = replace_buf.iter().collect();
                let count = self.search_matches.len();
                let s = match step { ReplaceStep::SearchInput => 0u8, ReplaceStep::ReplaceInput => 1u8 };
                (q, r, count, s)
            }
            _ => return,
        };
        if step_num == 0 {
            if q.is_empty() {
                self.status = "Replace — Search: ".into();
            } else if count == 0 {
                self.status = format!("Replace — Search: {}  (not found)  Enter=next  Esc=cancel", q);
            } else {
                self.status = format!("Replace — Search: {}  [{} found]  Enter=next  Esc=cancel", q, count);
            }
        } else {
            self.status = format!("Replace \"{}\" with: {}", q, r_str);
        }
    }

    // ── Symlink overwrite confirmation ────────────────────────────────────────

    fn handle_symlink_confirm_key(&mut self, code: KeyCode) -> io::Result<bool> {
        let path = match &self.prompt {
            Prompt::SymlinkConfirm(p) => p.clone(),
            _ => return Ok(false),
        };
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.prompt = Prompt::None;
                self.write_file(path)?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.prompt = Prompt::None;
                self.status = "Save cancelled.".into();
            }
            _ => {}
        }
        Ok(true)
    }

    // ── Encoding / line-ending selector ──────────────────────────────────────

    fn handle_enc_select_key(&mut self, code: KeyCode) -> io::Result<bool> {
        let step = match self.prompt {
            Prompt::EncodingSelect(s) => s,
            _ => return Ok(false),
        };

        match step {
            EncStep::ChooseEncoding => match code {
                KeyCode::Esc => {
                    self.prompt = Prompt::None;
                    self.status = "Cancelled.".into();
                }
                KeyCode::Char('1') => {
                    self.prompt = Prompt::EncodingSelect(EncStep::ChooseLineEnding(FileEncoding::Utf8));
                    self.status = "Line ending: [L] LF  [C] CRLF  Esc: Cancel".into();
                }
                KeyCode::Char('2') => {
                    self.prompt = Prompt::EncodingSelect(EncStep::ChooseLineEnding(FileEncoding::Utf8Bom));
                    self.status = "Line ending: [L] LF  [C] CRLF  Esc: Cancel".into();
                }
                KeyCode::Char('3') => {
                    self.prompt = Prompt::EncodingSelect(EncStep::ChooseLineEnding(FileEncoding::ShiftJis));
                    self.status = "Line ending: [L] LF  [C] CRLF  Esc: Cancel".into();
                }
                KeyCode::Char('4') => {
                    self.prompt = Prompt::EncodingSelect(EncStep::ChooseLineEnding(FileEncoding::EucJp));
                    self.status = "Line ending: [L] LF  [C] CRLF  Esc: Cancel".into();
                }
                _ => {}
            },
            EncStep::ChooseLineEnding(enc) => match code {
                KeyCode::Esc => {
                    self.prompt = Prompt::None;
                    self.status = "Cancelled.".into();
                }
                KeyCode::Char('l') | KeyCode::Char('L') => {
                    self.file_encoding = enc;
                    self.line_ending = LineEnding::Lf;
                    self.prompt = Prompt::None;
                    self.status = format!("Set to {} / LF — Ctrl+S to save.", enc.label());
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.file_encoding = enc;
                    self.line_ending = LineEnding::Crlf;
                    self.prompt = Prompt::None;
                    self.status = format!("Set to {} / CRLF — Ctrl+S to save.", enc.label());
                }
                _ => {}
            },
        }
        Ok(true)
    }

    // ── Selection / clipboard ─────────────────────────────────────────────────

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

    fn clipboard_to_string(&self) -> String {
        self.clipboard
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sync_to_sys_clipboard(&mut self) {
        let text = self.clipboard_to_string();
        if let Some(ref mut cb) = self.sys_clipboard {
            let _ = cb.set_text(text);
        }
    }

    fn paste(&mut self) {
        let sys_text = self
            .sys_clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok());

        if let Some(text) = sys_text {
            self.clipboard = text.lines().map(|l| l.chars().collect()).collect();
            if self.clipboard.is_empty() {
                self.clipboard.push(vec![]);
            }
        }

        if self.clipboard.is_empty() { return; }
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

    fn wrap_step(tw: usize, logical_x: usize, col_x: usize, vis_row: usize, ch: char) -> (usize, usize) {
        let is_tab = ch == '\t';
        let w = if is_tab {
            TAB_WIDTH - (logical_x % TAB_WIDTH)
        } else {
            ch.width().unwrap_or(0)
        };
        let avail = tw.saturating_sub(col_x);
        if !is_tab && avail > 0 && avail < w {
            (vis_row + 1, w)
        } else if col_x + w >= tw {
            let overflow = col_x + w - tw;
            if overflow == 0 { (vis_row + 1, 0) } else { (vis_row + 1, overflow) }
        } else {
            (vis_row, col_x + w)
        }
    }

    fn visual_height(&self, row: usize) -> usize {
        if !self.word_wrap { return 1; }
        let tw = self.term_cols as usize;
        if tw == 0 { return 1; }
        let mut vis_row = 0usize;
        let mut col_x = 0usize;
        let mut logical_x = 0usize;
        for ch in &self.lines[row] {
            let (vr, cx) = Self::wrap_step(tw, logical_x, col_x, vis_row, *ch);
            logical_x += if *ch == '\t' { TAB_WIDTH - (logical_x % TAB_WIDTH) } else { ch.width().unwrap_or(0) };
            vis_row = vr;
            col_x = cx;
        }
        vis_row + 1
    }

    fn wrap_render_pos(&self, row: usize, col: usize) -> (usize, usize) {
        let tw = self.term_cols as usize;
        if tw == 0 { return (0, 0); }
        let mut vis_row = 0usize;
        let mut col_x = 0usize;
        let mut logical_x = 0usize;
        for ch in &self.lines[row][..col.min(self.lines[row].len())] {
            let w = if *ch == '\t' { TAB_WIDTH - (logical_x % TAB_WIDTH) } else { ch.width().unwrap_or(0) };
            let (vr, cx) = Self::wrap_step(tw, logical_x, col_x, vis_row, *ch);
            logical_x += w;
            vis_row = vr;
            col_x = cx;
        }
        if col < self.lines[row].len() {
            let ch = self.lines[row][col];
            if ch != '\t' {
                let w = ch.width().unwrap_or(0);
                let avail = tw.saturating_sub(col_x);
                if avail > 0 && avail < w {
                    return (vis_row + 1, 0);
                }
            }
        }
        (vis_row, col_x)
    }

    fn adjust_scroll_wrap(&mut self) {
        let text_rows = self.text_rows();
        if self.row < self.top {
            self.top = self.row;
            return;
        }
        loop {
            let rows_before: usize =
                (self.top..self.row).map(|r| self.visual_height(r)).sum();
            let (cursor_vis, _) = self.wrap_render_pos(self.row, self.col);
            if rows_before + cursor_vis < text_rows { break; }
            if self.top >= self.row { break; }
            self.top += 1;
        }
    }

    fn char_boundary_le(&self, row: usize, target: usize) -> usize {
        let mut x = 0usize;
        for ch in &self.lines[row] {
            let w = if *ch == '\t' {
                TAB_WIDTH - (x % TAB_WIDTH)
            } else {
                ch.width().unwrap_or(0)
            };
            if x + w > target { return x; }
            x += w;
        }
        x
    }

    fn char_boundary_ge(&self, row: usize, target: usize) -> usize {
        let mut x = 0usize;
        for ch in &self.lines[row] {
            let w = if *ch == '\t' {
                TAB_WIDTH - (x % TAB_WIDTH)
            } else {
                ch.width().unwrap_or(0)
            };
            if x >= target { return x; }
            x += w;
        }
        x
    }

    pub fn adjust_left(&mut self) {
        if self.word_wrap {
            self.left = 0;
            self.adjust_scroll_wrap();
            return;
        }
        let cursor_disp = self.display_col(self.row, self.col) as usize;
        let screen_w = self.term_cols as usize;
        if screen_w == 0 { return; }
        if cursor_disp < self.left {
            self.left = self.char_boundary_le(self.row, cursor_disp);
        } else if cursor_disp >= self.left + screen_w {
            let min_left = cursor_disp.saturating_sub(screen_w - 1);
            self.left = self.char_boundary_ge(self.row, min_left);
        }
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
        if self.col > 0 || self.row > 0 { self.push_undo(); }
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
            if self.top > self.row { self.top = self.row; }
        }
    }

    fn delete(&mut self) {
        self.selection = None;
        let line_len = self.lines[self.row].len();
        let can_delete = self.col < line_len || self.row + 1 < self.lines.len();
        if can_delete { self.push_undo(); }
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
            if self.row < self.top { self.top = self.row; }
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
            if self.row >= self.top + self.text_rows() { self.top += 1; }
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
            if self.row < self.top { self.top = self.row; }
        }
    }

    fn move_right(&mut self) {
        let line_len = self.lines[self.row].len();
        if self.col < line_len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
            if self.row >= self.top + self.text_rows() { self.top += 1; }
        }
    }

    fn move_home(&mut self) { self.col = 0; }
    fn move_end(&mut self) { self.col = self.lines[self.row].len(); }

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

        // cur_attr: (selected, is_tab, search_level)
        // search_level: 0=none, 1=match, 2=current match
        type Attr = (bool, bool, u8);
        let reset_attr: Attr = (false, false, 0);

        // Helper: emit escape sequences for attribute transition
        macro_rules! apply_attr {
            ($stdout:expr, $cur:expr, $new:expr) => {{
                let new: Attr = $new;
                if new != *$cur {
                    queue!($stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                    let (sel, tab, srch) = new;
                    if sel {
                        queue!($stdout, Print(crossterm::style::SetAttribute(Attribute::Reverse)))?;
                    } else if srch == 2 {
                        queue!($stdout, SetBackgroundColor(Color::Yellow), SetForegroundColor(Color::Black))?;
                    } else if srch == 1 {
                        queue!($stdout, SetBackgroundColor(Color::DarkYellow), SetForegroundColor(Color::Black))?;
                    }
                    if tab && !sel && srch == 0 {
                        queue!($stdout, SetForegroundColor(Color::DarkGrey))?;
                    }
                    *$cur = new;
                }
            }};
        }

        // ── Text area ─────────────────────────────────────────────────────────
        if self.word_wrap {
            let tw = self.term_cols as usize;
            let mut screen_row = 0usize;
            let mut doc_row = self.top;

            while screen_row < text_rows {
                if doc_row >= self.lines.len() {
                    queue!(
                        stdout,
                        cursor::MoveTo(0, screen_row as u16),
                        terminal::Clear(ClearType::CurrentLine),
                        Print('~')
                    )?;
                    screen_row += 1;
                    continue;
                }

                let mut logical_x = 0usize;
                let mut vis_row = 0usize;
                let mut col_x = 0usize;
                let mut cur_attr: Attr = reset_attr;

                macro_rules! next_vis_row {
                    ($stdout:expr) => {{
                        if cur_attr != reset_attr {
                            queue!($stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                            cur_attr = reset_attr;
                        }
                        vis_row += 1;
                        col_x = 0;
                        if screen_row + vis_row < text_rows {
                            queue!(
                                $stdout,
                                cursor::MoveTo(0, (screen_row + vis_row) as u16),
                                terminal::Clear(ClearType::CurrentLine)
                            )?;
                        }
                    }};
                }

                queue!(
                    stdout,
                    cursor::MoveTo(0, screen_row as u16),
                    terminal::Clear(ClearType::CurrentLine)
                )?;

                for (char_idx, ch) in self.lines[doc_row].iter().enumerate() {
                    if screen_row + vis_row >= text_rows { break; }

                    let is_tab = *ch == '\t';
                    let w = if is_tab {
                        TAB_WIDTH - (logical_x % TAB_WIDTH)
                    } else {
                        ch.width().unwrap_or(0)
                    };
                    logical_x += w;

                    let avail = tw.saturating_sub(col_x);

                    if !is_tab && avail > 0 && avail < w {
                        // Wide char doesn't fit: pad → move to next row
                        let sel = self.in_selection(doc_row, char_idx);
                        let srch = self.search_attr_at(doc_row, char_idx);
                        apply_attr!(stdout, &mut cur_attr, (sel, false, srch));
                        for _ in 0..avail { queue!(stdout, Print(' '))?; }
                        next_vis_row!(stdout);
                        if screen_row + vis_row < text_rows {
                            let sel2 = self.in_selection(doc_row, char_idx);
                            let srch2 = self.search_attr_at(doc_row, char_idx);
                            apply_attr!(stdout, &mut cur_attr, (sel2, false, srch2));
                            queue!(stdout, Print(ch))?;
                            col_x = w;
                        }
                    } else if col_x + w > tw {
                        // Tab (or overflow) spans boundary
                        let fits = avail;
                        let overflow = w - fits;
                        let sel = self.in_selection(doc_row, char_idx);
                        let srch = self.search_attr_at(doc_row, char_idx);
                        apply_attr!(stdout, &mut cur_attr, (sel, false, srch));
                        for _ in 0..fits { queue!(stdout, Print(' '))?; }
                        next_vis_row!(stdout);
                        if screen_row + vis_row < text_rows {
                            let sel2 = self.in_selection(doc_row, char_idx);
                            let srch2 = self.search_attr_at(doc_row, char_idx);
                            apply_attr!(stdout, &mut cur_attr, (sel2, false, srch2));
                            for _ in 0..overflow { queue!(stdout, Print(' '))?; }
                            col_x = overflow;
                        }
                    } else {
                        // Normal: render in place
                        let sel = self.in_selection(doc_row, char_idx);
                        let srch = self.search_attr_at(doc_row, char_idx);
                        apply_attr!(stdout, &mut cur_attr, (sel, is_tab, srch));
                        if is_tab {
                            queue!(stdout, Print('>'))?;
                            for _ in 1..w { queue!(stdout, Print(' '))?; }
                        } else {
                            queue!(stdout, Print(ch))?;
                        }
                        col_x += w;
                        if col_x == tw {
                            next_vis_row!(stdout);
                        }
                    }
                }

                if cur_attr != reset_attr {
                    queue!(stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                }

                screen_row += vis_row + 1;
                doc_row += 1;
            }
        } else {
            // ── Horizontal-scroll mode ────────────────────────────────────────
            for screen_row in 0..text_rows {
                let doc_row = self.top + screen_row;
                queue!(
                    stdout,
                    cursor::MoveTo(0, screen_row as u16),
                    terminal::Clear(ClearType::CurrentLine)
                )?;

                if doc_row < self.lines.len() {
                    let mut display_x: usize = 0;
                    let screen_w = self.term_cols as usize;
                    let mut cur_attr: Attr = reset_attr;

                    for (char_idx, ch) in self.lines[doc_row].iter().enumerate() {
                        let is_tab = *ch == '\t';
                        let w = if is_tab {
                            TAB_WIDTH - (display_x % TAB_WIDTH)
                        } else {
                            ch.width().unwrap_or(0)
                        };

                        if display_x + w <= self.left {
                            display_x += w;
                            continue;
                        }
                        if display_x < self.left {
                            let visible_w = (display_x + w - self.left).min(screen_w);
                            let sel = self.in_selection(doc_row, char_idx);
                            let srch = self.search_attr_at(doc_row, char_idx);
                            apply_attr!(stdout, &mut cur_attr, (sel, false, srch));
                            for _ in 0..visible_w { queue!(stdout, Print(' '))?; }
                            display_x += w;
                            continue;
                        }
                        let screen_x = display_x - self.left;
                        if screen_x >= screen_w { break; }
                        let avail = screen_w - screen_x;

                        let sel = self.in_selection(doc_row, char_idx);
                        let srch = self.search_attr_at(doc_row, char_idx);
                        apply_attr!(stdout, &mut cur_attr, (sel, is_tab, srch));
                        if is_tab {
                            let print_w = w.min(avail);
                            queue!(stdout, Print('>'))?;
                            for _ in 1..print_w { queue!(stdout, Print(' '))?; }
                        } else if w > avail {
                            for _ in 0..avail { queue!(stdout, Print(' '))?; }
                        } else {
                            queue!(stdout, Print(ch))?;
                        }
                        display_x += w;
                    }
                    if cur_attr != reset_attr {
                        queue!(stdout, ResetColor, Print(crossterm::style::SetAttribute(Attribute::Reset)))?;
                    }
                } else {
                    queue!(stdout, Print('~'))?;
                }
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
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .unwrap_or_else(|| "[New File]".into());
        let dirty = if self.modified { " [+]" } else { "" };
        let enc_info = format!("{}  {}", self.file_encoding.label(), self.line_ending.label());
        let pos = format!(" {}:{} ", self.row + 1, self.col + 1);
        let left = format!(" {}{}  {}", file_name, dirty, enc_info);
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
            Prompt::None
            | Prompt::QuitConfirm
            | Prompt::EncodingSelect(_)
            | Prompt::SymlinkConfirm(_) => {
                let cursor_disp = self.display_col(self.row, self.col) as usize;
                let (cursor_screen_row, cursor_screen_col) = if self.word_wrap {
                    let rows_before: usize =
                        (self.top..self.row).map(|r| self.visual_height(r)).sum();
                    let (vrow, vcol) = self.wrap_render_pos(self.row, self.col);
                    (rows_before + vrow, vcol)
                } else {
                    (self.row - self.top, cursor_disp.saturating_sub(self.left))
                };
                queue!(
                    stdout,
                    cursor::MoveTo(cursor_screen_col as u16, cursor_screen_row as u16)
                )?;
            }
            Prompt::SaveAs { buf, cur } => {
                let prefix_len = "Save as: ".len() as u16;
                let typed_width: u16 = buf[..*cur]
                    .iter()
                    .map(|c| c.width().unwrap_or(0) as u16)
                    .sum();
                queue!(stdout, cursor::MoveTo(prefix_len + typed_width, self.term_rows - 1))?;
            }
            Prompt::Search { buf, cur } => {
                let prefix_len = "Search: ".len() as u16;
                let typed_width: u16 = buf[..*cur]
                    .iter()
                    .map(|c| c.width().unwrap_or(0) as u16)
                    .sum();
                queue!(stdout, cursor::MoveTo(prefix_len + typed_width, self.term_rows - 1))?;
            }
            Prompt::Replace { search_buf, replace_buf, search_cur, replace_cur, step } => {
                let (prefix_str, buf, cur) = match step {
                    ReplaceStep::SearchInput => {
                        ("Replace — Search: ", search_buf as &Vec<char>, *search_cur)
                    }
                    ReplaceStep::ReplaceInput => {
                        // prefix = "Replace \"<query>\" with: "
                        // We compute the displayed prefix length from the status string.
                        // Status is set to: Replace "<q>" with: <r>
                        // cursor goes after the prefix, before replacement text.
                        // We'll handle this case separately below.
                        ("", replace_buf as &Vec<char>, *replace_cur)
                    }
                };
                match step {
                    ReplaceStep::SearchInput => {
                        let prefix_len = prefix_str.len() as u16;
                        let typed_width: u16 = buf[..cur]
                            .iter()
                            .map(|c| c.width().unwrap_or(0) as u16)
                            .sum();
                        queue!(stdout, cursor::MoveTo(prefix_len + typed_width, self.term_rows - 1))?;
                    }
                    ReplaceStep::ReplaceInput => {
                        let q: String = search_buf.iter().collect();
                        let prefix = format!("Replace \"{}\" with: ", q);
                        let prefix_len: u16 = prefix.chars()
                            .map(|c| c.width().unwrap_or(0) as u16)
                            .sum();
                        let typed_width: u16 = replace_buf[..*replace_cur]
                            .iter()
                            .map(|c| c.width().unwrap_or(0) as u16)
                            .sum();
                        queue!(stdout, cursor::MoveTo(prefix_len + typed_width, self.term_rows - 1))?;
                    }
                }
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

    editor.adjust_left();
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
                    if editor.pending_quit { break 'main; }
                } else {
                    match (code, modifiers) {
                        // ── Quit ─────────────────────────────────────────────
                        (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                            if editor.modified {
                                editor.prompt = Prompt::QuitConfirm;
                                editor.status =
                                    "Unsaved changes!  [S]ave and quit  [U]nsave and quit  [C]ancel".into();
                            } else {
                                break 'main;
                            }
                        }

                        // ── Save / Save As ────────────────────────────────────
                        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                            editor.save()?;
                        }
                        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                            editor.start_save_as();
                        }

                        // ── Undo ─────────────────────────────────────────────
                        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
                            editor.undo();
                        }

                        // ── Select All ───────────────────────────────────────
                        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                            editor.select_all();
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
                                        "Copy: move cursor to selection end, then Ctrl+C again.".into();
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
                                        "Switched to copy mode. Press Ctrl+C again to copy.".into();
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
                                        "Cut: move cursor to selection end, then Ctrl+X again.".into();
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
                                        "Switched to cut mode. Press Ctrl+X again to cut.".into();
                                }
                            }
                        }

                        // ── Paste (Ctrl+V) ────────────────────────────────────
                        (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                            editor.selection = None;
                            editor.paste();
                        }

                        // ── Encoding / line-ending selector ───────────────────
                        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                            editor.prompt = Prompt::EncodingSelect(EncStep::ChooseEncoding);
                            editor.status = format!(
                                "Encoding: [1] UTF-8  [2] UTF-8 BOM  [3] Shift-JIS  [4] EUC-JP  Esc: Cancel  (current: {})",
                                editor.file_encoding.label()
                            );
                        }

                        // ── Search (Ctrl+F) ───────────────────────────────────
                        (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                            // Pre-fill with previous query if any
                            let prefill = editor.search_query.clone();
                            let cur = prefill.len();
                            editor.prompt = Prompt::Search { buf: prefill, cur };
                            editor.update_search();
                            if !editor.search_matches.is_empty() {
                                let idx = editor.find_nearest_match_from(editor.row, editor.col);
                                editor.jump_to_match(idx);
                            }
                            editor.status = editor.search_status();
                        }

                        // ── Replace (Ctrl+R) ──────────────────────────────────
                        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                            let prefill = editor.search_query.clone();
                            let cur = prefill.len();
                            editor.prompt = Prompt::Replace {
                                search_buf: prefill,
                                replace_buf: vec![],
                                search_cur: cur,
                                replace_cur: 0,
                                step: ReplaceStep::SearchInput,
                            };
                            editor.update_search();
                            editor.update_replace_status();
                        }

                        // ── Word wrap toggle (Ctrl+B) ─────────────────────────
                        (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                            editor.word_wrap = !editor.word_wrap;
                            editor.left = 0;
                            editor.status = if editor.word_wrap {
                                "Word wrap: ON".into()
                            } else {
                                "Word wrap: OFF".into()
                            };
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

        editor.adjust_left();
        editor.render(&mut stdout)?;
    }

    if keyboard_enhanced {
        execute!(stdout, PopKeyboardEnhancementFlags)?;
    }
    execute!(stdout, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    Ok(())
}
