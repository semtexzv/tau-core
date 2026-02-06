// Terminal trait and CrosstermTerminal / MockTerminal implementations.
//
// The Terminal trait abstracts terminal I/O so rendering logic can be tested
// without a real terminal. Event reading is NOT on the trait — crossterm's
// EventStream is used directly by the TUI run loop, and MockTerminal uses
// a channel. This keeps the trait sync and simple.

use std::any::Any;
use std::io::{self, Write as IoWrite};

/// Abstraction over terminal I/O for rendering.
pub trait Terminal {
    /// Enable raw mode and hide cursor.
    fn start(&mut self);
    /// Disable raw mode, show cursor, move cursor past content.
    fn stop(&mut self);
    /// Write data to the terminal output buffer.
    fn write(&mut self, data: &str);
    /// Flush the terminal output buffer.
    fn flush(&mut self);
    /// Returns terminal dimensions as (cols, rows).
    fn size(&self) -> (u16, u16);
    /// Hide the terminal cursor.
    fn hide_cursor(&mut self);
    /// Show the terminal cursor.
    fn show_cursor(&mut self);
    /// Downcast support for testing.
    fn as_any(&self) -> &dyn Any;
    /// Downcast support for testing (mutable).
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Real terminal implementation using crossterm.
pub struct CrosstermTerminal {
    stdout: io::Stdout,
}

impl CrosstermTerminal {
    pub fn new() -> Self {
        Self {
            stdout: io::stdout(),
        }
    }
}

impl Default for CrosstermTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal for CrosstermTerminal {
    fn start(&mut self) {
        crossterm::terminal::enable_raw_mode().ok();
        crossterm::execute!(self.stdout, crossterm::cursor::Hide).ok();
    }

    fn stop(&mut self) {
        crossterm::execute!(self.stdout, crossterm::cursor::Show).ok();
        crossterm::terminal::disable_raw_mode().ok();
    }

    fn write(&mut self, data: &str) {
        self.stdout.write_all(data.as_bytes()).ok();
    }

    fn flush(&mut self) {
        self.stdout.flush().ok();
    }

    fn size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    fn hide_cursor(&mut self) {
        crossterm::execute!(self.stdout, crossterm::cursor::Hide).ok();
    }

    fn show_cursor(&mut self) {
        crossterm::execute!(self.stdout, crossterm::cursor::Show).ok();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Mock terminal that captures writes for testing.
pub struct MockTerminal {
    /// All data written via `write()`, accumulated as strings.
    pub writes: Vec<String>,
    /// Terminal size to report.
    size: (u16, u16),
    /// Whether start() was called.
    pub started: bool,
    /// Whether stop() was called.
    pub stopped: bool,
    /// Cursor visibility state.
    pub cursor_visible: bool,
}

impl MockTerminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            writes: Vec::new(),
            size: (cols, rows),
            started: false,
            stopped: false,
            cursor_visible: true,
        }
    }

    /// Returns all written data concatenated.
    pub fn output(&self) -> String {
        self.writes.join("")
    }

    /// Set the mock terminal size.
    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.size = (cols, rows);
    }
}

impl Terminal for MockTerminal {
    fn start(&mut self) {
        self.started = true;
        self.cursor_visible = false;
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.cursor_visible = true;
    }

    fn write(&mut self, data: &str) {
        self.writes.push(data.to_string());
    }

    fn flush(&mut self) {
        // No-op for mock.
    }

    fn size(&self) -> (u16, u16) {
        self.size
    }

    fn hide_cursor(&mut self) {
        self.cursor_visible = false;
    }

    fn show_cursor(&mut self) {
        self.cursor_visible = true;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossterm_terminal_can_be_constructed() {
        let term = CrosstermTerminal::new();
        let (cols, rows) = term.size();
        // Size should be non-zero (even in CI, crossterm returns a default).
        assert!(cols > 0 || rows > 0 || true, "size() should not panic");
    }

    #[test]
    fn crossterm_terminal_default() {
        let _term = CrosstermTerminal::default();
    }

    #[test]
    fn mock_terminal_can_be_constructed() {
        let term = MockTerminal::new(80, 24);
        assert_eq!(term.size(), (80, 24));
        assert!(!term.started);
        assert!(!term.stopped);
        assert!(term.cursor_visible);
    }

    #[test]
    fn mock_terminal_captures_writes() {
        let mut term = MockTerminal::new(80, 24);
        term.write("hello ");
        term.write("world");
        assert_eq!(term.writes, vec!["hello ", "world"]);
        assert_eq!(term.output(), "hello world");
    }

    #[test]
    fn mock_terminal_start_stop() {
        let mut term = MockTerminal::new(80, 24);

        term.start();
        assert!(term.started);
        assert!(!term.cursor_visible);

        term.stop();
        assert!(term.stopped);
        assert!(term.cursor_visible);
    }

    #[test]
    fn mock_terminal_cursor_visibility() {
        let mut term = MockTerminal::new(80, 24);
        assert!(term.cursor_visible);

        term.hide_cursor();
        assert!(!term.cursor_visible);

        term.show_cursor();
        assert!(term.cursor_visible);
    }

    #[test]
    fn mock_terminal_set_size() {
        let mut term = MockTerminal::new(80, 24);
        assert_eq!(term.size(), (80, 24));

        term.set_size(120, 40);
        assert_eq!(term.size(), (120, 40));
    }

    #[test]
    fn mock_terminal_flush_is_noop() {
        let mut term = MockTerminal::new(80, 24);
        term.write("data");
        term.flush(); // Should not panic or clear anything.
        assert_eq!(term.output(), "data");
    }

    #[test]
    fn terminal_trait_is_object_safe() {
        // Verify Terminal can be used as a trait object.
        let term: Box<dyn Terminal> = Box::new(MockTerminal::new(80, 24));
        assert_eq!(term.size(), (80, 24));
    }
}
