//! Interactive arrow-key/mouse pickers on crossterm, used only when stdin and
//! stderr are real terminals. The classic numbered menus in [`crate::menus`]
//! remain the fallback (pipes, scripts, CI) and the unit-tested path.
//!
//! The picker is a small custom implementation rather than a prompt library:
//! the popular libraries (inquire, dialoguer, cliclack) have no mouse
//! support at all, and mouse is the point here — hover highlights the row
//! under the pointer, a click selects it, the wheel moves the highlight.
//! The state machine, the event mapping and the frame rendering are pure
//! functions over [`SelectState`] — fully unit-tested without a terminal;
//! only the event loop itself needs a real TTY and is verified manually.

use crate::models::Model;
use crossterm::cursor::{Hide, MoveTo, MoveToColumn, MoveUp, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use std::fmt;
use std::io::{IsTerminal, Write};

/// True when stdin (read by crossterm) and stderr (where the picker renders)
/// are both real terminals.
pub fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Outcome of one interactive prompt.
pub enum Pick<T> {
    /// The user picked a value.
    Chosen(T),
    /// Terminal vanished mid-run: the caller falls back to the classic text
    /// menu (which reads EOF → defaults).
    NotTty,
    /// Esc.
    Canceled,
    /// Ctrl+C.
    Interrupted,
    /// I/O or configuration failure.
    Error(String),
}

impl<T> Pick<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Pick<U> {
        match self {
            Pick::Chosen(v) => Pick::Chosen(f(v)),
            Pick::NotTty => Pick::NotTty,
            Pick::Canceled => Pick::Canceled,
            Pick::Interrupted => Pick::Interrupted,
            Pick::Error(e) => Pick::Error(e),
        }
    }
}

// ── Backend selection ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BackendOption {
    pub id: &'static str,
    pub label: &'static str,
}

impl fmt::Display for BackendOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

/// Same three entries and descriptions as `menus::choose_backend`.
pub fn backend_options() -> Vec<BackendOption> {
    vec![
        BackendOption {
            id: "openai",
            label: "openai — OpenAI models (ChatGPT subscription or API key, local proxy)",
        },
        BackendOption {
            id: "opencode",
            label: "opencode — OpenCode gateway (x-api-key)",
        },
        BackendOption {
            id: "anthropic",
            label: "anthropic — stock Claude Code (pass-through, no changes)",
        },
    ]
}

/// Index of the last used backend (pre-highlighted), else the first entry.
pub fn backend_cursor(default: &str) -> usize {
    backend_options()
        .iter()
        .position(|o| o.id == default)
        .unwrap_or(0)
}

pub fn choose_backend(default: &str) -> Pick<String> {
    let options = backend_options();
    let start = backend_cursor(default);
    run_picker("Choose a backend:", &options, start, 3, |o| {
        o.id.to_string()
    })
    .map(|i| options[i].id.to_string())
}

// ── Model selection ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ModelOption {
    pub slug: String,
    pub display: String,
    pub context: u64,
    pub last_used: bool,
}

impl fmt::Display for ModelOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::menus::model_label(
            &self.display,
            self.context,
            self.last_used,
        ))
    }
}

pub fn choose_model(title: &str, models: &[Model], default: &str, last_used: &str) -> Pick<String> {
    let options: Vec<ModelOption> = models
        .iter()
        .map(|m| ModelOption {
            slug: m.slug.clone(),
            display: m.display.clone(),
            context: m.context,
            last_used: m.slug == last_used,
        })
        .collect();
    let start = models.iter().position(|m| m.slug == default).unwrap_or(0);
    run_picker(title, &options, start, 10, |o| o.display.clone()).map(|i| options[i].slug.clone())
}

// ── Effort (reasoning level) selection ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum EffortChoice {
    Level(String),
    /// Sentinel option, offered when the last used level is not valid for
    /// this model; maps back to "" (the model's own default applies
    /// upstream), exactly like the text menu's empty answer.
    ModelDefault(String),
}

impl fmt::Display for EffortChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EffortChoice::Level(v) => f.write_str(v),
            EffortChoice::ModelDefault(label) => f.write_str(label),
        }
    }
}

/// Builds the option list and the starting cursor.
/// - last used valid: levels only, cursor on the last used level;
/// - otherwise: the sentinel "use the model default [(dflt)]" first, cursor
///   on it.
pub fn effort_options(
    valid: &[String],
    last: &str,
    model_default: &str,
) -> (Vec<EffortChoice>, usize) {
    if !last.is_empty() && valid.iter().any(|v| v == last) {
        let cursor = valid.iter().position(|v| v == last).unwrap_or(0);
        let opts = valid
            .iter()
            .map(|v| EffortChoice::Level(v.clone()))
            .collect();
        (opts, cursor)
    } else {
        let label = if model_default.is_empty() {
            "use the model default".to_string()
        } else {
            format!("use the model default ({model_default})")
        };
        let mut opts = vec![EffortChoice::ModelDefault(label)];
        opts.extend(valid.iter().map(|v| EffortChoice::Level(v.clone())));
        (opts, 0)
    }
}

pub fn choose_effort(valid: &[String], last: &str, model_default: &str) -> Pick<String> {
    let (options, start) = effort_options(valid, last, model_default);
    run_picker("Reasoning level:", &options, start, 6, |o| o.to_string()).map(|i| {
        match &options[i] {
            EffortChoice::Level(v) => v.clone(),
            EffortChoice::ModelDefault(_) => String::new(),
        }
    })
}

// ── ChatGPT login confirmation ─────────────────────────────────────────────────

pub fn ask_login() -> Pick<bool> {
    let options = ["Yes", "No"];
    run_picker("Log in now (device flow via Codex)?", &options, 1, 2, |o| {
        o.to_string()
    })
    .map(|i| i == 0)
}

// ── Pure picker state machine ──────────────────────────────────────────────────

/// The picker state: the (possibly filtered) option list, the cursor position
/// and the scroll window. All operations are pure; rendering and event
/// handling live in [`render_frame`] and [`handle_event`].
pub struct SelectState<'a, T> {
    options: &'a [T],
    filtered: Vec<usize>,
    cursor: usize,
    scroll: usize,
    page: usize,
    filter: String,
}

impl<'a, T: fmt::Display> SelectState<'a, T> {
    pub fn new(options: &'a [T], starting_cursor: usize, page: usize) -> Self {
        let cursor = starting_cursor.min(options.len().saturating_sub(1));
        SelectState {
            options,
            filtered: (0..options.len()).collect(),
            cursor,
            scroll: (cursor / page.max(1)) * page.max(1),
            page: page.max(1),
            filter: String::new(),
        }
    }

    /// The typed filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Number of visible (filtered) options.
    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// Cursor position within the filtered list.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Row of the cursor within the scroll window (for rendering).
    pub fn cursor_row(&self) -> usize {
        self.cursor.saturating_sub(self.scroll)
    }

    /// The visible window: indices into `filtered` from `scroll` up to
    /// `scroll + page`.
    pub fn visible(&self) -> &[usize] {
        let end = (self.scroll + self.page).min(self.filtered.len());
        &self.filtered[self.scroll..end]
    }

    /// Number of visible rows (window length).
    pub fn window_len(&self) -> usize {
        self.visible().len()
    }

    /// The selected option's index into the original list, if any.
    pub fn selected_index(&self) -> Option<usize> {
        self.filtered.get(self.cursor).copied()
    }

    /// The selected option's original slice element, if any.
    pub fn selected(&self) -> Option<&T> {
        self.selected_index().map(|i| &self.options[i])
    }

    /// Moves the cursor by `delta` rows, clamping at the list bounds and
    /// scrolling the window to keep the cursor visible.
    pub fn move_cursor(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        let target = (self.cursor as i32 + delta).clamp(0, len - 1) as usize;
        self.cursor = target;
        self.ensure_visible();
    }

    /// Moves the cursor by one page (or to the list bounds).
    pub fn page_move(&mut self, delta: i32) {
        self.move_cursor(delta * self.page as i32);
    }

    /// Moves the cursor to an absolute index in the filtered list (Home/End).
    pub fn move_to(&mut self, index: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        self.cursor = index.clamp(0, len - 1) as usize;
        self.ensure_visible();
    }

    /// Replaces the filter (case-insensitive substring match on the option
    /// labels) and resets the cursor to the first match.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        let needle = filter.to_lowercase();
        self.filtered = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| o.to_string().to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Clears the filter, restoring the full list.
    pub fn clear_filter(&mut self) {
        self.set_filter("");
    }

    fn ensure_visible(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.page {
            self.scroll = self.cursor + 1 - self.page;
        }
    }
}

/// What an event did to the picker.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    /// Enter or a left click on an option: the selection is final.
    Selected,
    /// Esc with an empty filter.
    Canceled,
    /// Ctrl+C.
    Interrupted,
}

/// Maps a click row (0-based, screen coordinates) to a row within the scroll
/// window, or `None` when the click landed outside the option rows. The
/// prompt origin is the screen row where the title renders; option row 0 is
/// one below it.
pub fn click_to_index(click_row: u16, origin_row: u16, window_len: usize) -> Option<usize> {
    let row = click_row as i32 - origin_row as i32 - 1;
    if row >= 0 && (row as usize) < window_len {
        Some(row as usize)
    } else {
        None
    }
}

/// Applies one terminal event to the picker state. `origin_row` is the screen
/// row where the prompt was first drawn (for click mapping).
pub fn handle_event<T: fmt::Display>(
    state: &mut SelectState<T>,
    event: &Event,
    origin_row: u16,
) -> Outcome {
    match event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) if *kind == KeyEventKind::Press => match code {
            KeyCode::Up => {
                state.move_cursor(-1);
                Outcome::Continue
            }
            KeyCode::Down => {
                state.move_cursor(1);
                Outcome::Continue
            }
            KeyCode::PageUp => {
                state.page_move(-1);
                Outcome::Continue
            }
            KeyCode::PageDown => {
                state.page_move(1);
                Outcome::Continue
            }
            KeyCode::Home => {
                state.move_to(0);
                Outcome::Continue
            }
            KeyCode::End => {
                state.move_to(i32::MAX);
                Outcome::Continue
            }
            KeyCode::Enter => {
                if state.selected_index().is_some() {
                    Outcome::Selected
                } else {
                    Outcome::Continue
                }
            }
            // Esc with a non-empty filter only clears the filter.
            KeyCode::Esc => {
                if state.filter().is_empty() {
                    Outcome::Canceled
                } else {
                    state.clear_filter();
                    Outcome::Continue
                }
            }
            KeyCode::Backspace => {
                let mut f = state.filter().to_string();
                f.pop();
                state.set_filter(&f);
                Outcome::Continue
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Outcome::Interrupted,
            KeyCode::Char(ch)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut f = state.filter().to_string();
                f.push(*ch);
                state.set_filter(&f);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        },
        Event::Mouse(me) => match me.kind {
            // A click on an option selects it immediately.
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = click_to_index(me.row, origin_row, state.window_len()) {
                    state.move_to(state.scroll as i32 + row as i32);
                    Outcome::Selected
                } else {
                    Outcome::Continue
                }
            }
            // Hover: move the highlight to the row under the pointer
            // (any-motion tracking, mode 1003, is enabled by
            // EnableMouseCapture).
            MouseEventKind::Moved => {
                if let Some(row) = click_to_index(me.row, origin_row, state.window_len()) {
                    state.move_to(state.scroll as i32 + row as i32);
                }
                Outcome::Continue
            }
            MouseEventKind::ScrollUp => {
                state.move_cursor(-1);
                Outcome::Continue
            }
            MouseEventKind::ScrollDown => {
                state.move_cursor(1);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        },
        _ => Outcome::Continue,
    }
}

/// Renders the prompt frame (title line + visible options with the cursor
/// row highlighted). No help line: the hint is deliberately omitted.
pub fn render_frame<T: fmt::Display>(title: &str, state: &SelectState<T>) -> String {
    let mut s = String::new();
    s.push_str("\x1b[32m?\x1b[0m ");
    s.push_str(title);
    if !state.filter().is_empty() {
        s.push_str(&format!("  \x1b[90m{}\x1b[0m", state.filter()));
    }
    s.push_str("\r\n");
    let win = state.visible();
    if win.is_empty() {
        s.push_str("  \x1b[90m(no matches)\x1b[0m");
        return s;
    }
    let cursor_row = state.cursor_row();
    for (i, &idx) in win.iter().enumerate() {
        let label = state.options[idx].to_string();
        if i == cursor_row {
            s.push_str(&format!("\x1b[1;36m> \x1b[0m{label}"));
        } else {
            s.push_str(&format!("  {label}"));
        }
        if i + 1 < win.len() {
            s.push_str("\r\n");
        }
    }
    s
}

// ── Terminal loop ──────────────────────────────────────────────────────────────

/// Restores the terminal when the picker exits, on every path (Drop).
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stderr(), Show, DisableMouseCapture);
    }
}

/// Number of terminal rows `frame` occupies.
fn frame_height(frame: &str) -> usize {
    frame.matches("\r\n").count() + 1
}

/// Moves back to the prompt origin and clears everything below it.
fn clear_prompt(out: &mut impl Write, last_height: usize) {
    let _ = if last_height > 1 {
        execute!(
            out,
            MoveUp((last_height - 1) as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
    } else {
        execute!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown))
    };
}

/// The screen row where the cursor sits right now (0-based), used to map
/// mouse clicks to option rows.
///
/// crossterm's `cursor::position()` writes its DSR query to stdout, which
/// stalls ~2 s when stdout is redirected; it is skipped in that case
/// (keyboard navigation is unaffected). On Windows the stderr console buffer
/// is read directly — no query, works with redirected stdout.
fn origin_row() -> Option<u16> {
    #[cfg(unix)]
    {
        if std::io::stdout().is_terminal() {
            crossterm::cursor::position().ok().map(|(_, row)| row)
        } else {
            None
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_ERROR_HANDLE,
        };
        let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        if handle.is_null() {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            return None;
        }
        let y = info.dwCursorPosition.Y;
        let top = unsafe { info.srWindow.Top };
        Some((y.saturating_sub(top)) as u16)
    }
}

/// Runs one interactive picker over `options` (drawn on stderr, raw mode +
/// mouse capture enabled) and returns the chosen index, or the abort reason.
/// `answer` produces the text shown on the final answer line.
fn run_picker<T: fmt::Display>(
    title: &str,
    options: &[T],
    starting_cursor: usize,
    page: usize,
    answer: impl Fn(&T) -> String,
) -> Pick<usize> {
    if options.is_empty() {
        return Pick::Error("no options".to_string());
    }
    if terminal::enable_raw_mode().is_err() {
        return Pick::NotTty;
    }
    let guard = TermGuard;
    let mut out = std::io::stderr();
    if execute!(out, Hide, EnableMouseCapture).is_err() {
        return Pick::Error("cannot set up the terminal".to_string());
    }
    let mut state = SelectState::new(options, starting_cursor.min(options.len() - 1), page);

    // Resolve the prompt's origin (the screen row where the title renders)
    // so mouse rows map to option rows. The first frame is drawn where the
    // cursor is, then the position is read back: that row is origin +
    // height - 1, so the read-back also accounts for any scroll the first
    // draw caused (prompt near the screen bottom). When the position query
    // is unavailable (redirected stdout, unresponsive terminal), the prompt
    // is anchored at the bottom of the screen instead, where the origin is
    // deterministic and clicks keep working.
    let frame0 = render_frame(title, &state);
    let h = frame_height(&frame0);
    let origin = match origin_row() {
        Some(row) => {
            if out.write_all(frame0.as_bytes()).is_err() || out.flush().is_err() {
                return Pick::NotTty;
            }
            match origin_row() {
                Some(row) => row.saturating_sub(h as u16 - 1),
                None => row,
            }
        }
        None => {
            let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
            let anchor = height.saturating_sub(h as u16 + 1);
            let _ = execute!(out, MoveTo(0, anchor));
            if out.write_all(frame0.as_bytes()).is_err() || out.flush().is_err() {
                return Pick::NotTty;
            }
            anchor
        }
    };
    let mut last_height = h;
    loop {
        // Redraw the prompt at its origin.
        let frame = render_frame(title, &state);
        clear_prompt(&mut out, last_height);
        if out.write_all(frame.as_bytes()).is_err() || out.flush().is_err() {
            return Pick::NotTty;
        }
        last_height = frame_height(&frame);

        let event = match event::read() {
            Ok(e) => e,
            Err(_) => return Pick::NotTty,
        };
        match handle_event(&mut state, &event, origin) {
            Outcome::Continue => {}
            Outcome::Selected => {
                let label = answer(state.selected().expect("Selected implies an option"));
                clear_prompt(&mut out, last_height);
                // The titles end with ":", so trim it before adding the
                // answer separator.
                let t = title.strip_suffix(':').unwrap_or(title);
                let _ = writeln!(out, "\x1b[32m?\x1b[0m {t}: {label}");
                drop(guard);
                return Pick::Chosen(state.selected_index().expect("Selected implies an option"));
            }
            Outcome::Canceled => {
                clear_prompt(&mut out, last_height);
                return Pick::Canceled;
            }
            Outcome::Interrupted => {
                clear_prompt(&mut out, last_height);
                return Pick::Interrupted;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, MouseEvent};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn state<'a>(options: &'a [String], start: usize) -> SelectState<'a, String> {
        SelectState::new(options, start, 3)
    }

    fn mouse(kind: MouseEventKind, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    // ── SelectState ──

    #[test]
    fn new_clamps_starting_cursor_and_keeps_window_around_it() {
        let opts = ["a", "b", "c", "d", "e"].map(String::from);
        let s = state(&opts, 4);
        assert_eq!(s.cursor(), 4);
        assert_eq!(s.visible(), &[3, 4]); // page 3 window at scroll 3
        assert_eq!(s.cursor_row(), 1);
        let s = state(&opts, 99);
        assert_eq!(s.cursor(), 4);
    }

    #[test]
    fn cursor_moves_and_scrolls_the_window() {
        let opts = ["a", "b", "c", "d", "e"].map(String::from);
        let mut s = state(&opts, 0);
        s.move_cursor(-1); // clamped at the top
        assert_eq!(s.cursor(), 0);
        for _ in 0..4 {
            s.move_cursor(1);
        }
        assert_eq!(s.cursor(), 4);
        s.move_cursor(1); // clamped at the bottom
        assert_eq!(s.cursor(), 4);
        // Moving beyond the window edge scrolls it.
        let mut s = state(&opts, 0);
        s.move_cursor(3);
        assert_eq!((s.cursor(), s.scroll, s.cursor_row()), (3, 1, 2));
        s.move_cursor(1);
        assert_eq!((s.cursor(), s.scroll, s.cursor_row()), (4, 2, 2));
        s.move_cursor(-3);
        assert_eq!((s.cursor(), s.scroll, s.cursor_row()), (1, 1, 0));
    }

    #[test]
    fn filter_matches_case_insensitively_and_resets_cursor() {
        let opts = ["gpt-5.6-sol", "gpt-5.4-mini", "minimax-m3", "kimi-k3"].map(String::from);
        let mut s = state(&opts, 3);
        s.set_filter("GPT");
        assert_eq!(s.visible(), &[0, 1]);
        assert_eq!(s.cursor(), 0);
        s.set_filter("k3");
        assert_eq!(s.visible(), &[3]); // only kimi-k3 contains "k3"
        s.set_filter("nope");
        assert!(s.is_empty());
        s.clear_filter();
        assert_eq!(s.len(), 4);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn filter_matches_the_display_label() {
        // ModelOption's Display includes context and the "(last used)" tag.
        let opt = ModelOption {
            slug: "gpt-one".into(),
            display: "GPT-One".into(),
            context: 828_400,
            last_used: true,
        };
        let opts = [opt];
        let mut s = SelectState::new(&opts, 0, 3);
        s.set_filter("828K");
        assert_eq!(s.len(), 1);
        s.set_filter("last used");
        assert_eq!(s.len(), 1);
    }

    // ── click_to_index ──

    #[test]
    fn click_maps_option_rows_only() {
        let origin = 4; // title at row 4, first option at row 5
        assert_eq!(click_to_index(4, origin, 3), None); // title row
        assert_eq!(click_to_index(5, origin, 3), Some(0));
        assert_eq!(click_to_index(6, origin, 3), Some(1));
        assert_eq!(click_to_index(7, origin, 3), Some(2));
        assert_eq!(click_to_index(8, origin, 3), None); // below the window
        assert_eq!(click_to_index(3, origin, 3), None); // above the title
    }

    // ── handle_event ──

    #[test]
    fn keyboard_moves_and_selects() {
        let opts = ["a", "b", "c"].map(String::from);
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Down), 0),
            Outcome::Continue
        );
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Down), 0),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 2);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Up), 0),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 1);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Enter), 0),
            Outcome::Selected
        );
        assert_eq!(s.selected_index(), Some(1));
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Home), 0),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 0);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::End), 0),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 2);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::PageDown), 0),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 2); // clamped
    }

    #[test]
    fn escape_cancels_or_clears_the_filter() {
        let opts = ["a", "b", "c"].map(String::from);
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Esc), 0),
            Outcome::Canceled
        );
        // With a filter active, Esc only clears it.
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Char('b')), 0),
            Outcome::Continue
        );
        assert_eq!(s.visible(), &[1]);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Esc), 0),
            Outcome::Continue
        );
        assert_eq!(s.len(), 3);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Esc), 0),
            Outcome::Canceled
        );
    }

    #[test]
    fn backspace_edits_the_filter() {
        let opts = ["gpt-5", "kimi-k3"].map(String::from);
        let mut s = state(&opts, 0);
        handle_event(&mut s, &key(KeyCode::Char('g')), 0);
        handle_event(&mut s, &key(KeyCode::Char('p')), 0);
        handle_event(&mut s, &key(KeyCode::Char('t')), 0);
        assert_eq!(s.visible(), &[0]);
        handle_event(&mut s, &key(KeyCode::Backspace), 0);
        assert_eq!(s.filter(), "gp");
        assert_eq!(s.visible(), &[0]); // "gp" still matches "gpt-5"
        handle_event(&mut s, &key(KeyCode::Backspace), 0);
        handle_event(&mut s, &key(KeyCode::Backspace), 0);
        assert_eq!(s.filter(), "");
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn ctrl_c_interrupts() {
        let opts = ["a"].map(String::from);
        let mut s = state(&opts, 0);
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(handle_event(&mut s, &ev, 0), Outcome::Interrupted);
        // Plain 'c' is filter input.
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(&mut s, &key(KeyCode::Char('c')), 0),
            Outcome::Continue
        );
    }

    #[test]
    fn key_release_events_are_ignored() {
        let opts = ["a", "b"].map(String::from);
        let mut s = state(&opts, 0);
        let ev = Event::Key(KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(handle_event(&mut s, &ev, 0), Outcome::Continue);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn hover_moves_the_highlight_without_selecting() {
        let opts = ["a", "b", "c", "d"].map(String::from);
        let origin = 2; // options at rows 3,4,5 (page 3)
        let mut s = state(&opts, 0);
        // Hover over option 2 (row 5).
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::Moved, 5), origin),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 2);
        assert!(s.selected_index().is_some()); // still selected, just highlighted
                                               // Hovering the title or below the window leaves the cursor alone.
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::Moved, 2), origin),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 0);
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::Moved, 6), origin),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 0);
        // Hover then click selects the hovered row.
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::Moved, 5), origin),
            Outcome::Continue
        );
        assert_eq!(
            handle_event(
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Left), 5),
                origin
            ),
            Outcome::Selected
        );
        assert_eq!(s.selected_index(), Some(2));
    }

    #[test]
    fn mouse_click_selects_and_wheel_moves() {
        let opts = ["a", "b", "c", "d"].map(String::from);
        let origin = 2; // options at rows 3,4,5 (page 3)
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Left), 5),
                origin
            ),
            Outcome::Selected
        );
        assert_eq!(s.selected_index(), Some(2));
        // Click outside the window rows does nothing.
        let mut s = state(&opts, 0);
        assert_eq!(
            handle_event(
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Left), 2),
                origin
            ),
            Outcome::Continue
        );
        assert_eq!(
            handle_event(
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Left), 6),
                origin
            ),
            Outcome::Continue
        );
        // Wheel moves the cursor.
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::ScrollDown, 0), origin),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 1);
        assert_eq!(
            handle_event(&mut s, &mouse(MouseEventKind::ScrollUp, 0), origin),
            Outcome::Continue
        );
        assert_eq!(s.cursor(), 0);
        // A right-button click is ignored.
        assert_eq!(
            handle_event(
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Right), 3),
                origin
            ),
            Outcome::Continue
        );
    }

    // ── render_frame ──

    #[test]
    fn frame_renders_title_cursor_and_no_hint() {
        let opts = ["alpha", "beta"].map(String::from);
        let s = state(&opts, 1);
        let frame = render_frame("Pick:", &s);
        assert!(frame.starts_with("\x1b[32m?\x1b[0m Pick:\r\n"));
        assert!(
            frame.contains("\x1b[1;36m> \x1b[0mbeta"),
            "cursor on the highlighted row"
        );
        assert!(frame.contains("  alpha"));
        assert!(!frame.contains("up/down"), "no help hint");
        assert!(!frame.contains("enter"), "no help hint");
        // Title + two options = three rows.
        assert_eq!(frame_height(&frame), 3);
    }

    #[test]
    fn frame_shows_the_filter_and_no_matches() {
        let opts = ["alpha", "beta"].map(String::from);
        let mut s = state(&opts, 0);
        s.set_filter("alp");
        let frame = render_frame("Pick:", &s);
        assert!(frame.contains("\x1b[90malp\x1b[0m"));
        s.set_filter("zzz");
        let frame = render_frame("Pick:", &s);
        assert!(frame.contains("(no matches)"));
    }

    // ── Option builders (kept from the inquire version) ──

    #[test]
    fn backend_cursor_finds_the_default() {
        assert_eq!(backend_cursor("openai"), 0);
        assert_eq!(backend_cursor("opencode"), 1);
        assert_eq!(backend_cursor("anthropic"), 2);
        assert_eq!(backend_cursor("nope"), 0);
    }

    #[test]
    fn effort_options_preserve_text_menu_semantics() {
        let valid: Vec<String> = ["low", "high"].iter().map(|s| s.to_string()).collect();
        let (opts, cursor) = effort_options(&valid, "high", "medium");
        assert_eq!(
            opts,
            vec![
                EffortChoice::Level("low".into()),
                EffortChoice::Level("high".into())
            ]
        );
        assert_eq!(cursor, 1);
        let (opts, cursor) = effort_options(&valid, "turbo", "medium");
        assert_eq!(
            opts,
            vec![
                EffortChoice::ModelDefault("use the model default (medium)".into()),
                EffortChoice::Level("low".into()),
                EffortChoice::Level("high".into())
            ]
        );
        assert_eq!(cursor, 0);
        let (opts, cursor) = effort_options(&valid, "turbo", "");
        assert_eq!(
            opts[0],
            EffortChoice::ModelDefault("use the model default".into())
        );
        assert_eq!(cursor, 0);
    }
}
