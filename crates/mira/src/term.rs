//! A terminal, hand-rolled on `libc`.
//!
//! ratatui is the obvious answer, and it costs 61 crates — a 54% increase on a
//! tree whose size (113 crates, 4.5 MB) is a stated property of the product.
//! What it buys over this file is a constraint-solving layout engine and a
//! damage-tracked cell buffer; the TUI here has fixed panes and redraws one
//! screenful per keystroke. So: `termios` for raw mode, `TIOCGWINSZ` for the
//! size, `poll(2)` for input, ANSI for the rest. `libc` is already in the tree
//! for `statfs`.
//!
//! Unix only, which is the same bet `mmap`, `SIGTERM` and the filesystem guard
//! already make.

use std::io::{self, Read, Write};
use std::sync::OnceLock;

/// The terminal settings as they were before [`Term::enter`].
///
/// A static rather than a field because the panic hook has to reach them, and
/// the hook outlives any borrow we could hand it. `panic = "abort"` in the
/// release profile does not skip hooks — it skips *unwinding* — so this is
/// still the last thing that runs before a crash, and without it a panic leaves
/// the shell in raw mode with no echo and no cursor.
static ORIG: OnceLock<libc::termios> = OnceLock::new();

pub struct Term {
    /// Bytes read from stdin but not yet consumed as a key.
    ///
    /// One `read` can return a whole escape sequence, several keystrokes from a
    /// fast typist, or half of either. Without this, the tail of a burst is
    /// silently dropped.
    pending: Vec<u8>,
    out: io::BufWriter<io::Stdout>,
}

impl Term {
    pub fn enter() -> io::Result<Term> {
        if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "not a terminal; the TUI needs stdin and stdout on a tty",
            ));
        }
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = ORIG.set(t);

        let mut raw = t;
        unsafe { libc::cfmakeraw(&mut raw) };
        // VMIN 0 / VTIME 0: `read` returns immediately with whatever is there.
        // Blocking is `poll`'s job, and doing it in both places is how a
        // keystroke ends up waiting for the next one.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));

        let mut out = io::BufWriter::new(io::stdout());
        // Alternate screen, then hide the cursor. Leaving on the alternate
        // screen is what puts the user's scrollback back the way they left it.
        out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J")?;
        out.flush()?;
        Ok(Term {
            pending: Vec::with_capacity(64),
            out,
        })
    }

    /// Visible size, re-read every frame.
    ///
    /// Polling beats a `SIGWINCH` handler here: the draw loop already wakes on a
    /// timer, and a signal handler would need a `static` flag to communicate
    /// with it anyway.
    pub fn size(&self) -> (usize, usize) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ as _, &mut ws) } != 0 || ws.ws_col == 0 {
            return (80, 24);
        }
        (ws.ws_col as usize, ws.ws_row as usize)
    }

    /// Paint one frame: home the cursor, write every row, erase whatever the
    /// last frame left below.
    ///
    /// A full repaint rather than a diff. At 200×60 that is 12 000 cells, and
    /// one `write` of 30 KB costs less than tracking which of them changed.
    pub fn draw(&mut self, rows: &[String]) -> io::Result<()> {
        self.out.write_all(b"\x1b[H")?;
        for row in rows {
            self.out.write_all(row.as_bytes())?;
            self.out.write_all(b"\x1b[K\r\n")?;
        }
        self.out.write_all(b"\x1b[J")?;
        self.out.flush()
    }

    /// Next key, or `None` if `timeout_ms` passed with nothing to read.
    pub fn key(&mut self, timeout_ms: i32) -> io::Result<Option<Key>> {
        loop {
            match parse(&mut self.pending) {
                Parsed::Key(k) => return Ok(Some(k)),
                Parsed::Need => {}
            }
            // A lone ESC and the start of an arrow key are the same first byte.
            // The only way to tell them apart is that the rest of the sequence
            // is already in flight, so wait a beat before calling it ESC.
            let wait = if self.pending.is_empty() {
                timeout_ms
            } else {
                20
            };
            if !poll_in(wait)? {
                return Ok(match self.pending.is_empty() {
                    true => None,
                    // Incomplete after the grace period: emit the first byte for
                    // what it is and resynchronise rather than wedging.
                    false => Some(take_one(&mut self.pending)),
                });
            }
            let mut buf = [0u8; 256];
            let n = io::stdin().read(&mut buf)?;
            if n == 0 {
                return Ok(Some(Key::Ctrl('c')));
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
    }
}

enum Parsed {
    Key(Key),
    Need,
}

/// One key off the front of `b`, or [`Parsed::Need`] if what is there could
/// still grow into a longer sequence.
///
/// Free rather than a method so the tests can drive it with a plain `Vec`: a
/// `Term` restores the terminal when it drops, and constructing one in a test
/// sprays escape codes across the test runner's output.
fn parse(b: &mut Vec<u8>) -> Parsed {
    let Some(&first) = b.first() else {
        return Parsed::Need;
    };
    let one = |b: &mut Vec<u8>, k| {
        b.remove(0);
        Parsed::Key(k)
    };
    match first {
        0x1b => {
            match b.get(1) {
                None => Parsed::Need,
                // CSI: parameters, then one byte in 0x40..=0x7e ends it.
                Some(b'[') => {
                    let Some(end) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
                        return Parsed::Need;
                    };
                    let seq: Vec<u8> = b[2..2 + end + 1].to_vec();
                    b.drain(..3 + end);
                    Parsed::Key(csi(&seq))
                }
                // SS3, which is what some terminals send for the arrows in
                // application cursor mode.
                Some(b'O') => match b.get(2) {
                    None => Parsed::Need,
                    Some(&c) => {
                        b.drain(..3);
                        Parsed::Key(csi(&[c]))
                    }
                },
                // Alt+key. Nothing here binds one, so drop the modifier and keep
                // the key rather than swallowing both.
                Some(_) => {
                    b.remove(0);
                    Parsed::Need
                }
            }
        }
        b'\r' | b'\n' => one(b, Key::Enter),
        b'\t' => one(b, Key::Tab),
        0x7f | 0x08 => one(b, Key::Backspace),
        c if c < 0x20 => one(b, Key::Ctrl((c + b'a' - 1) as char)),
        c if c < 0x80 => one(b, Key::Char(c as char)),
        c => {
            // UTF-8 continuation bytes may not have arrived yet.
            let len = match c {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            if b.len() < len {
                return Parsed::Need;
            }
            let s = String::from_utf8_lossy(&b[..len]).into_owned();
            b.drain(..len);
            Parsed::Key(s.chars().next().map_or(Key::Esc, Key::Char))
        }
    }
}

fn take_one(b: &mut Vec<u8>) -> Key {
    match b.remove(0) {
        0x1b => Key::Esc,
        c if c < 0x20 => Key::Ctrl((c + b'a' - 1) as char),
        c => Key::Char(c as char),
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        restore();
    }
}

fn csi(seq: &[u8]) -> Key {
    match seq {
        b"A" => Key::Up,
        b"B" => Key::Down,
        b"C" => Key::Right,
        b"D" => Key::Left,
        b"H" | b"1~" | b"7~" => Key::Home,
        b"F" | b"4~" | b"8~" => Key::End,
        b"5~" => Key::PageUp,
        b"6~" => Key::PageDown,
        b"Z" => Key::BackTab,
        _ => Key::Esc,
    }
}

/// Put the terminal back. Idempotent, because both `Drop` and the panic hook
/// can reach it and a double panic would otherwise run it twice.
fn restore() {
    if let Some(t) = ORIG.get() {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
}

fn poll_in(timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if n >= 0 {
        return Ok(n > 0);
    }
    let e = io::Error::last_os_error();
    // A resize interrupts the poll. Reporting "nothing to read" sends the caller
    // back around the draw loop, which re-reads the size — which is exactly the
    // handling a resize wants, so it is not retried here.
    match e.kind() {
        io::ErrorKind::Interrupted => Ok(false),
        _ => Err(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Home,
    End,
    PageUp,
    PageDown,
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const REV: &str = "\x1b[7m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";

/// One line of the frame, built left to right with its visible width tracked
/// separately from its bytes.
///
/// Styling is inline ANSI, so `buf.len()` says nothing about how wide the line
/// renders; every truncation and every pad has to go through `width`. That is
/// the entire reason this type exists rather than a `String`.
pub struct Row {
    buf: String,
    width: usize,
    max: usize,
}

impl Row {
    pub fn new(max: usize) -> Row {
        Row {
            buf: String::with_capacity(max + 32),
            width: 0,
            max,
        }
    }

    pub fn left(&self) -> usize {
        self.max.saturating_sub(self.width)
    }

    /// Move the right edge, so a right-aligned field can reserve its space
    /// before the left-hand side is written and be clipped away by it.
    pub fn cap(&mut self, max: usize) -> &mut Row {
        self.max = max;
        self
    }

    /// Append text, truncated to what is left of the line.
    ///
    /// ponytail: one column per `char`. A CJK log body renders one cell wide per
    /// character here and will run past the right edge; the fix is a
    /// `unicode-width` table, which is a crate, and the payload this reads is
    /// attribute keys and metric names. Revisit if a user shows up with a
    /// wide-character body.
    pub fn put(&mut self, style: &str, s: &str) -> &mut Row {
        let left = self.left();
        if left == 0 {
            return self;
        }
        if !style.is_empty() {
            self.buf.push_str(style);
        }
        let mut n = 0;
        for c in s.chars() {
            if n == left {
                break;
            }
            // A raw control byte in a log body would move the cursor.
            self.buf.push(if (c as u32) < 0x20 { '·' } else { c });
            n += 1;
        }
        if !style.is_empty() {
            self.buf.push_str(RESET);
        }
        self.width += n;
        self
    }

    pub fn plain(&mut self, s: &str) -> &mut Row {
        self.put("", s)
    }

    /// Append an already-rendered line of known visible `width`, byte for byte.
    ///
    /// [`put`](Row::put) would rewrite its ESC bytes to `·` and count each one
    /// as a column, which is right for a log body and wrong for a line that came
    /// out of another `Row`. Nothing here can recover the width from the bytes,
    /// so the caller states it — pass a line finished with [`done`](Row::done),
    /// which pads to exactly the width it was built for.
    pub fn raw(&mut self, s: &str, width: usize) -> &mut Row {
        self.buf.push_str(s);
        self.width += width;
        self
    }

    /// Pad with spaces until the cursor sits at `col`. Never truncates: a
    /// column that overflowed its slot pushes the next one right rather than
    /// losing it.
    pub fn pad_to(&mut self, col: usize) -> &mut Row {
        while self.width < col.min(self.max) {
            self.buf.push(' ');
            self.width += 1;
        }
        self
    }

    /// Repeat `c` `n` times, clipped to the line.
    pub fn repeat(&mut self, style: &str, c: char, n: usize) -> &mut Row {
        let n = n.min(self.left());
        if n == 0 {
            return self;
        }
        if !style.is_empty() {
            self.buf.push_str(style);
        }
        for _ in 0..n {
            self.buf.push(c);
        }
        if !style.is_empty() {
            self.buf.push_str(RESET);
        }
        self.width += n;
        self
    }

    /// Finish the line, padded to full width so a selected row's reverse-video
    /// background reaches the right edge.
    pub fn fill(mut self, style: &str) -> String {
        if !style.is_empty() {
            // Re-open the style over the padding only; the text has already
            // closed its own.
            self.buf.push_str(style);
        }
        while self.width < self.max {
            self.buf.push(' ');
            self.width += 1;
        }
        self.buf.push_str(RESET);
        self.buf
    }

    pub fn done(self) -> String {
        self.fill("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Width accounting has to ignore the escape sequences, or every styled
    /// line silently truncates early.
    #[test]
    fn styling_does_not_count_against_the_width() {
        let mut r = Row::new(10);
        r.put(RED, "abc").plain("de");
        assert_eq!(r.left(), 5);
        let s = r.done();
        assert!(s.contains(RED));
        // 10 visible columns, whatever the byte length.
        let visible: String = strip(&s);
        assert_eq!(visible, "abcde     ");
    }

    #[test]
    fn text_is_clipped_at_the_edge_and_control_bytes_are_defanged() {
        let mut r = Row::new(6);
        r.plain("a\nb").plain("xxxxxxxx");
        assert_eq!(strip(&r.done()), "a·bxxx");
    }

    #[test]
    fn pad_to_never_moves_backwards() {
        let mut r = Row::new(12);
        r.plain("overlong").pad_to(4).plain("|");
        assert_eq!(strip(&r.done()), "overlong|   ");
    }

    /// The sequences the app actually binds, including the ones that arrive
    /// split across two reads.
    #[test]
    fn escape_sequences_decode_to_keys() {
        let mut pending: Vec<u8> = Vec::new();
        let mut keys = |bytes: &[u8]| {
            pending.extend_from_slice(bytes);
            let mut out = Vec::new();
            while let Parsed::Key(k) = parse(&mut pending) {
                out.push(k);
            }
            out
        };
        assert_eq!(keys(b"\x1b[A\x1b[B"), vec![Key::Up, Key::Down]);
        assert_eq!(keys(b"\x1b[5~\x1b[6~"), vec![Key::PageUp, Key::PageDown]);
        assert_eq!(keys(b"\x1bOD"), vec![Key::Left]);
        assert_eq!(
            keys(b"jk\r\x7f"),
            vec![Key::Char('j'), Key::Char('k'), Key::Enter, Key::Backspace]
        );
        assert_eq!(keys(b"\x03"), vec![Key::Ctrl('c')]);
        // Split mid-sequence: nothing until the rest lands.
        assert_eq!(keys(b"\x1b["), vec![]);
        assert_eq!(keys(b"C"), vec![Key::Right]);
        // Multi-byte UTF-8, likewise.
        assert_eq!(keys(&[0xc3]), vec![]);
        assert_eq!(keys(&[0xa9]), vec![Key::Char('é')]);
    }

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
