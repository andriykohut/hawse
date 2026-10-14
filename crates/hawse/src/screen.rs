//! The one writer to stderr while a terminal form is up. The client's panel sits at the bottom
//! of the screen and is redrawn in place, so anything else written there, a log line included,
//! has to take the panel off, write, and put it back.

use std::io::{self, Write};
use std::sync::Mutex;

use tracing_subscriber::fmt::MakeWriter;

use crate::terminal::columns;

/// The panel on the screen below the last line printed, with the cursor at the end of its last
/// row.
#[derive(Debug)]
pub struct Screen {
    rows: Vec<String>,
    /// The terminal's width when `rows` were drawn, 0 when it did not say.
    width: usize,
}

/// The lines a row of `cols` columns takes at `width`: a row drawn for a wider terminal is
/// rewrapped onto more when it narrows.
fn lines(cols: usize, width: usize) -> usize {
    if width == 0 {
        1
    } else {
        cols.div_ceil(width).max(1)
    }
}

impl Screen {
    pub const fn new() -> Self {
        Self {
            rows: Vec::new(),
            width: 0,
        }
    }

    /// Takes the panel off, leaving the cursor at the start of the line its first row was on.
    fn erase(&self, out: &mut impl Write, width: usize) -> io::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let up: usize = self
            .rows
            .iter()
            .map(|row| lines(columns(row), width))
            .sum::<usize>()
            - 1;
        out.write_all(b"\r")?;
        if up > 0 {
            write!(out, "\x1b[{up}A")?;
        }
        out.write_all(b"\x1b[J")
    }

    fn put(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(self.rows.join("\n").as_bytes())
    }

    /// Writes `line` above the panel, which may hold several lines of its own.
    pub fn print(&mut self, out: &mut impl Write, line: &str, width: usize) -> io::Result<()> {
        self.erase(out, width)?;
        writeln!(out, "{line}")?;
        self.put(out)?;
        out.flush()
    }

    /// Replaces the panel with `rows`, each already cut to fit `width`. Nothing is written when
    /// they are what is on the screen at that width.
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        rows: Vec<String>,
        width: usize,
    ) -> io::Result<()> {
        if rows == self.rows && width == self.width {
            return Ok(());
        }
        self.erase(out, width)?;
        self.rows = rows;
        self.width = width;
        self.put(out)?;
        out.flush()
    }

    /// Leaves the panel on the screen as it is and moves under it, for what follows the run.
    pub fn release(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.rows.is_empty() {
            out.write_all(b"\n")?;
        }
        self.rows.clear();
        out.flush()
    }
}

static STDERR: Mutex<Screen> = Mutex::new(Screen::new());

/// The terminal's columns and lines, each 0 when stderr is not a terminal or does not say.
pub fn size() -> (usize, usize) {
    rustix::termios::tcgetwinsize(io::stderr()).map_or((0, 0), |size| {
        (usize::from(size.ws_col), usize::from(size.ws_row))
    })
}

// A write that fails is dropped by each of these: `eprintln!` would panic on a stderr whose
// reader has gone, and take the task that answers SIGTERM with it.

/// Writes `line` to stderr, above the panel when there is one.
pub fn print(line: &str) {
    let mut screen = STDERR.lock().expect("screen lock");
    let _ = screen.print(&mut io::stderr().lock(), line, size().0);
}

/// Replaces the panel on stderr with `rows`, cut to fit `width`.
pub fn draw(rows: Vec<String>, width: usize) {
    let mut screen = STDERR.lock().expect("screen lock");
    let _ = screen.draw(&mut io::stderr().lock(), rows, width);
}

/// Leaves the panel on stderr as it is and moves under it.
pub fn release() {
    let mut screen = STDERR.lock().expect("screen lock");
    let _ = screen.release(&mut io::stderr().lock());
}

/// Gives `tracing` a writer per event that prints the event through `print` once it is whole,
/// so a redraw never splits a log line.
#[derive(Clone, Copy, Debug)]
pub struct Stderr;

/// One event's text, printed when it drops.
pub struct Logged(Vec<u8>);

impl MakeWriter<'_> for Stderr {
    type Writer = Logged;

    fn make_writer(&self) -> Logged {
        Logged(Vec::new())
    }
}

impl Write for Logged {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            print(String::from_utf8_lossy(&self.0).trim_end_matches('\n'));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|&line| line.to_owned()).collect()
    }

    fn written(out: &mut Vec<u8>) -> String {
        String::from_utf8(std::mem::take(out)).unwrap()
    }

    #[test]
    fn a_redraw_erases_the_rows_it_drew_and_an_equal_frame_writes_nothing() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen
            .draw(&mut out, rows(&["head", "", "row"]), 80)
            .unwrap();
        assert_eq!(written(&mut out), "head\n\nrow");
        screen
            .draw(&mut out, rows(&["head", "", "row"]), 80)
            .unwrap();
        assert_eq!(written(&mut out), "");
        screen
            .draw(&mut out, rows(&["head", "", "new"]), 80)
            .unwrap();
        assert_eq!(written(&mut out), "\r\x1b[2A\x1b[Jhead\n\nnew");
    }

    #[test]
    fn a_line_printed_lands_above_the_panel() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen.draw(&mut out, rows(&["head", "row"]), 80).unwrap();
        written(&mut out);
        screen.print(&mut out, "warn", 80).unwrap();
        assert_eq!(written(&mut out), "\r\x1b[1A\x1b[Jwarn\nhead\nrow");
    }

    #[test]
    fn a_line_of_several_lines_lands_above_the_panel_whole() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen.draw(&mut out, rows(&["head"]), 80).unwrap();
        written(&mut out);
        screen.print(&mut out, "error\n  at line 3", 80).unwrap();
        assert_eq!(written(&mut out), "\r\x1b[Jerror\n  at line 3\nhead");
    }

    #[test]
    fn with_no_panel_a_line_prints_as_it_is() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen.print(&mut out, "hello", 80).unwrap();
        assert_eq!(written(&mut out), "hello\n");
    }

    #[test]
    fn a_narrower_terminal_erases_the_lines_its_rows_rewrapped_onto() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen
            .draw(&mut out, rows(&["0123456789", "x"]), 80)
            .unwrap();
        written(&mut out);
        // 10 columns rewrap onto 3 lines of 4, and the last row keeps 1.
        screen.draw(&mut out, rows(&["a"]), 4).unwrap();
        assert_eq!(written(&mut out), "\r\x1b[3A\x1b[Ja");
    }

    #[test]
    fn released_the_panel_stays_and_what_follows_prints_under_it() {
        let (mut screen, mut out) = (Screen::new(), Vec::new());
        screen.draw(&mut out, rows(&["head"]), 80).unwrap();
        screen.release(&mut out).unwrap();
        screen.print(&mut out, "bye", 80).unwrap();
        assert_eq!(written(&mut out), "head\nbye\n");
    }
}
