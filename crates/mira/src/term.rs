//! A terminal, hand-rolled on `libc`.
//!
//! ratatui is the obvious answer, and adding it to this workspace resolves 35
//! crates that are not already here — a 30% increase on a tree whose size (117
//! crates, 4.73 MiB) is a stated property of the product.
//! What it buys over this file is a constraint-solving layout engine and a
//! damage-tracked cell buffer; the TUI here has fixed panes and redraws one
//! screenful per keystroke. So: `termios` for raw mode, `TIOCGWINSZ` for the
//! size, `poll(2)` for input, three `sigaction`s to survive a resize and a
//! `kill`, ANSI for the rest. `libc` is already in the tree for `statfs`.
//!
//! Unix only, which is the same bet `mmap`, `SIGTERM` and the filesystem guard
//! already make.

use std::io::{self, IsTerminal, Read, Write};
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
        // Checked before `tcgetattr`, which would otherwise report a redirected
        // stdin as `ENOTTY` — "Inappropriate ioctl for device" — and says nothing
        // at all about a redirected stdout, which instead fills the file with
        // escape sequences.
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "not a terminal; the TUI needs stdin and stdout on a tty",
            ));
        }
        // SAFETY: `termios` is integers and the `c_cc` byte array — no pointer,
        // no niche, no field for which zero is not a value — so all-zero is a
        // valid `termios` to hold until `tcgetattr` overwrites it.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `&mut t` is a live, correctly typed, initialised `termios`, and
        // `tcgetattr` writes nothing past its end.
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = ORIG.set(t);

        let mut raw = t;
        // SAFETY: `raw` is a copy of the struct `tcgetattr` just filled, so
        // `cfmakeraw` reads and writes only initialised fields of a live struct.
        unsafe { libc::cfmakeraw(&mut raw) };
        // VMIN 0 / VTIME 0: `read` returns immediately with whatever is there.
        // Blocking is `poll`'s job, and doing it in both places is how a
        // keystroke ends up waiting for the next one.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is fully initialised — copied out of `tcgetattr`, edited
        // field by field — and `tcsetattr` only reads it, for the duration of
        // the call.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));

        // SIGWINCH so a resize breaks the `poll` in `key` and the loop repaints
        // at the new size; SIGTERM and SIGHUP so a `kill` or a closed ssh
        // session gives the terminal back the way the panic hook does. Their
        // default dispositions — discard, and die on the spot — are both wrong
        // for a process that owns the screen.
        on_signal(libc::SIGWINCH, winch);
        on_signal(libc::SIGTERM, bail);
        on_signal(libc::SIGHUP, bail);

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
    /// The `SIGWINCH` handler [`enter`](Term::enter) installs records nothing;
    /// it exists only to interrupt the `poll` the draw loop is parked in. This
    /// call is what learns the new size, on the frame that interruption paints.
    pub fn size(&self) -> (usize, usize) {
        // SAFETY: `winsize` is four `u16`s, so zero is a valid value — and it
        // has to be, because a failing `ioctl` leaves the struct untouched and
        // the `ws_col == 0` arm below is what reads it back.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ is the request whose argument is `*mut winsize`,
        // which is exactly what `&mut ws` is. `ioctl` is variadic, so nothing
        // checks that pairing but this comment — a different request constant
        // with this argument is the way the call goes wrong.
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

/// Put the terminal back. Idempotent, because `Drop`, the panic hook and a
/// fatal signal can all reach it and a double panic would otherwise run it
/// twice.
///
/// Every call in here is async-signal-safe — `tcsetattr` and `write` are on
/// POSIX's list, and `OnceLock::get` is one atomic load — because [`bail`] runs
/// it from a signal handler. `io::stdout()` is the thing that would not do:
/// taking its lock in a handler that interrupted the thread already holding it
/// is a deadlock at exactly the moment the user wants their terminal back.
fn restore() {
    if let Some(t) = ORIG.get() {
        // SAFETY: `t` borrows the `termios` `enter` filled with `tcgetattr` and
        // nothing has written it since — `OnceLock` hands out no `&mut` after
        // `set`. Reached from the `bail` handler, and `tcsetattr` is on POSIX's
        // async-signal-safe list, so interrupting a `tcsetattr` with this one is
        // defined.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    const OFF: &[u8] = b"\x1b[?25h\x1b[?1049l";
    // SAFETY: `OFF` is a `'static` slice, so its pointer is valid for the
    // `OFF.len()` bytes claimed for as long as the program runs, and `write`
    // only reads them. Async-signal-safe, which is why this is not `stdout`.
    unsafe { libc::write(1, OFF.as_ptr().cast(), OFF.len()) };
}

/// Install `h` for `sig`, without `SA_RESTART`.
///
/// The flag is omitted because it buys nothing here, not because [`poll_in`]
/// needs it: `poll(2)` is on signal(7)'s list of calls the kernel never
/// restarts whatever the flag says, and the `read(2)` in the same loop runs
/// under VMIN 0 / VTIME 0, so it returns immediately and is never sitting in a
/// restartable wait either. What makes a resize wake `poll_in` is installing a
/// handler at all — SIGWINCH's default disposition is to discard the signal,
/// and a discarded signal interrupts nothing.
fn on_signal(sig: libc::c_int, h: unsafe extern "C" fn(libc::c_int)) {
    // SAFETY: `sigaction` is POD, and all-zero is its "no flags, SIG_DFL"
    // value — the one the two writes below then replace.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = h as usize;
    // SAFETY: `sa` is initialised and outlives the call, which copies it;
    // `sigemptyset` writes only `sa_mask`. The load-bearing part is `sa_flags`,
    // left at zero by the `zeroed` above: with SA_SIGINFO clear the kernel calls
    // `sa_sigaction` with the single `c_int` that `h`'s type declares, so
    // setting that flag without widening `h`'s signature is what would break
    // this. A null `oldact` means "do not report the previous disposition",
    // which `sigaction(2)` permits. Both handlers this is ever called with —
    // [`winch`] and [`bail`] — are async-signal-safe.
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// A resize. Nothing to do in the handler — the delivery itself is the message,
/// and it arrives as an `EINTR` in [`poll_in`].
unsafe extern "C" fn winch(_: libc::c_int) {}

/// Give the terminal back, then die.
///
/// Installing a handler removed the default disposition, so returning would
/// resume a process the user asked to end; `_exit` rather than `exit` because
/// the latter runs atexit handlers that are not async-signal-safe, and 128+n is
/// the status a shell reports for a signal death.
unsafe extern "C" fn bail(sig: libc::c_int) {
    restore();
    // SAFETY: `_exit` dereferences nothing and never returns. It is
    // async-signal-safe, which is the entire reason it is here rather than
    // `exit` — see the doc above.
    unsafe { libc::_exit(128 + sig) };
}

fn poll_in(timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `&mut p` points at exactly the one initialised `pollfd` the count
    // of `1` claims, it lives across the call, and `poll` writes only `revents`.
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if n >= 0 {
        return Ok(n > 0);
    }
    let e = io::Error::last_os_error();
    // A resize interrupts the poll, which is the only reason `enter` installs a
    // SIGWINCH handler at all — the default disposition discards it and this
    // arm would be unreachable. Reporting "nothing to read" sends the caller
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
        // Split mid-sequence: nothing until the rest lands. Both prefixes, since
        // CSI and SS3 have separate "could still grow" arms.
        assert_eq!(keys(b"\x1b["), vec![]);
        assert_eq!(keys(b"C"), vec![Key::Right]);
        assert_eq!(keys(b"\x1bO"), vec![]);
        assert_eq!(keys(b"A"), vec![Key::Up]);
        // Multi-byte UTF-8, likewise, at every lead-byte width — the width is
        // read off the lead byte, so a wrong row in that table eats the next key.
        assert_eq!(keys(&[0xc3]), vec![]);
        assert_eq!(keys(&[0xa9]), vec![Key::Char('é')]);
        assert_eq!(keys(&[0xe2, 0x82]), vec![]);
        assert_eq!(keys(&[0xac]), vec![Key::Char('€')]);
        assert_eq!(keys(&[0xf0, 0x9f, 0x98]), vec![]);
        assert_eq!(keys(&[0x80]), vec![Key::Char('😀')]);
    }

    /// Every sequence in the `csi` table, because terminals disagree about which
    /// one they send for the same physical key and the table exists to absorb
    /// that. An unrecognised one is `Esc`, not a dropped byte: the app treats
    /// `Esc` as "cancel", which is the safe reading of a key we do not know.
    #[test]
    fn one_key_arrives_in_as_many_spellings_as_there_are_terminals() {
        fn key(bytes: &[u8]) -> Key {
            match parse(&mut bytes.to_vec()) {
                Parsed::Key(k) => k,
                Parsed::Need => panic!("incomplete: {bytes:?}"),
            }
        }
        for (bytes, want) in [
            (&b"\x1b[H"[..], Key::Home),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[7~", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1b[4~", Key::End),
            (b"\x1b[8~", Key::End),
            (b"\x1b[Z", Key::BackTab),
            (b"\x1b[C", Key::Right),
            (b"\x1bOA", Key::Up),
            // Not in the table, and not worth guessing at.
            (b"\x1b[200~", Key::Esc),
            (b"\t", Key::Tab),
            (b"\x08", Key::Backspace),
            (b"\n", Key::Enter),
        ] {
            assert_eq!(key(bytes), want, "{bytes:?}");
        }

        // Alt+x: the modifier is dropped and the key kept, so the next call sees
        // a bare `x` rather than both bytes being swallowed.
        let mut b = b"\x1bx".to_vec();
        assert!(matches!(parse(&mut b), Parsed::Need));
        assert_eq!(key(&b), Key::Char('x'));

        // What `key` falls back to when a sequence never completes: the first
        // byte for what it is, then resynchronise. A lone ESC is the case that
        // matters — it is how the filter box gets cancelled.
        for (byte, want) in [
            (0x1b, Key::Esc),
            (0x03, Key::Ctrl('c')),
            (b'q', Key::Char('q')),
        ] {
            let mut b = vec![byte, b'!'];
            assert_eq!(take_one(&mut b), want);
            assert_eq!(b, b"!");
        }
    }

    /// The three things `Row` does that `put` does not: reserve space from the
    /// right, re-emit an already-rendered line without re-counting its escapes,
    /// and pad under a style so a selected row's background reaches the edge.
    #[test]
    fn a_row_composes_out_of_other_rows_without_recounting_their_escapes() {
        // A right-aligned field reserves its space by moving the right edge in,
        // writing the left side, then letting it back out.
        let mut r = Row::new(20);
        r.plain("left").cap(20).pad_to(12).plain("right");
        assert_eq!(strip(&r.done()), "left        right   ");

        // `raw` takes the caller's word for the width. `put` would have counted
        // the 4 escape bytes of RED as 4 columns and rewritten them to `·`.
        let inner = {
            let mut i = Row::new(5);
            i.put(RED, "ab");
            i.done()
        };
        let mut outer = Row::new(10);
        outer.raw(&inner, 5).plain("xy");
        let s = outer.done();
        assert!(s.contains(RED), "the inner styling survived byte for byte");
        assert_eq!(strip(&s), "ab   xy   ");

        // `repeat` clips like `put` does, and a zero-width repeat writes nothing
        // at all — not an empty style pair, which would still be bytes.
        let mut r = Row::new(4);
        r.repeat(DIM, '-', 99);
        assert_eq!(strip(&r.done()), "----");
        let mut r = Row::new(0);
        r.repeat(DIM, '-', 3).put(RED, "x");
        assert_eq!(r.done(), RESET, "nothing fits, so nothing is written");

        // `fill` re-opens the style over the padding: the highlight on a selected
        // row has to reach the right edge, not stop where the text does.
        let mut r = Row::new(6);
        r.plain("ab");
        let s = r.fill(REV);
        assert!(s.ends_with(&format!("{REV}    {RESET}")), "{s:?}");
    }

    /// The branch that fires in anger: `mira mira` in a pipeline, in CI, or
    /// under a process supervisor. The rest of the syscall half runs on a real
    /// pty in [`the_terminal_half_runs_against_a_real_pty`].
    #[test]
    fn the_tui_refuses_a_stdin_that_is_not_a_terminal() {
        let Err(e) = Term::enter() else {
            panic!("cargo test does not run on a tty")
        };
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(e.to_string().contains("needs stdin and stdout on a tty"));
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

    /// The name of the test below, as `--exact` wants it.
    const SELF: &str = "term::tests::the_terminal_half_runs_against_a_real_pty";

    /// Everything in this file that talks to a terminal, against a terminal.
    ///
    /// `enter`, `size`, `draw`, `key`, `poll_in` and `restore` all address fd 0
    /// and fd 1 directly, so the only honest way to run them is on a process
    /// whose fd 0 and fd 1 are a tty — and it must not be *this* process, whose
    /// stdin belongs to the test runner and whose termios the runner needs back.
    ///
    /// So: open a pty, re-exec this same test binary against the slave side, and
    /// drive it from the master. The child is the same instrumented binary, so
    /// its coverage counts; `cargo llvm-cov` merges the profraw it writes.
    ///
    /// This is CLAUDE.md's `script -q /dev/null` recipe with the pty opened in
    /// process, which is what makes it a test rather than a thing to run by hand.
    #[test]
    fn the_terminal_half_runs_against_a_real_pty() {
        if std::env::var_os("MIRA_PTY_CHILD").is_some() {
            return on_the_pty();
        }
        use std::os::fd::FromRawFd;
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};

        // SAFETY: flags by value, no pointer argument.
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(master >= 0, "{}", io::Error::last_os_error());
        // SAFETY: `master` is the fd `posix_openpt` just returned, proved not
        // -1 by the assert above and closed by nothing until `from_raw_fd`
        // below; both calls take it by value.
        assert_eq!(unsafe { libc::grantpt(master) }, 0);
        // SAFETY: as above.
        assert_eq!(unsafe { libc::unlockpt(master) }, 0);
        // SAFETY: `ptsname` returns a NUL-terminated string in a static buffer,
        // non-null because `master` is a pty master — `grantpt` returned 0 on
        // it one line up, which it does not for anything else. The buffer is
        // clobbered by the next `ptsname` on any thread, so the `CStr` is copied
        // into an owned `String` in this same expression and never held.
        let slave_path = unsafe { std::ffi::CStr::from_ptr(libc::ptsname(master)) }
            .to_str()
            .unwrap()
            .to_owned();

        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)
            .unwrap();
        // A size the child can assert on, so `size` is checked against something
        // it cannot have made up. Set on the slave: macOS refuses TIOCSWINSZ on
        // a master with no slave open yet.
        let ws = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let fd = std::os::fd::AsRawFd::as_raw_fd(&slave);
        assert_eq!(
            // SAFETY: TIOCSWINSZ is the request whose variadic argument is
            // `*const winsize`, which is what `&ws` is, and `fd` is borrowed
            // from `slave`, still open here — it is dropped after the spawn.
            unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", SELF, "--nocapture"])
            .env("MIRA_PTY_CHILD", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            // Not the pty: the child's failures have to come back over a channel
            // the child cannot also be scribbling frames onto.
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(slave);

        // SAFETY: `from_raw_fd` takes ownership, and this is the only owner
        // `master` ever gets — it is still open (nothing has closed it since
        // `posix_openpt`), and the child holds dups of the *slave*, not this fd.
        // `rd` and its `try_clone` are now what will close it.
        let mut rd = unsafe { std::fs::File::from_raw_fd(master) };
        let mut wr = rd.try_clone().unwrap();
        let screen = Arc::new(Mutex::new(String::new()));
        let sink = screen.clone();
        // Drained on a thread: a child that fills the pty buffer while this side
        // is blocked writing to it is a deadlock, not a slow test.
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = rd.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });

        let arrived = |marker: &str| {
            (0..600).any(|_| {
                if screen.lock().unwrap().contains(marker) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            })
        };

        // Every keystroke waits for the marker the child draws once it has
        // consumed the previous one. Not politeness: a lone Esc only resolves as
        // Esc because the 20ms grace period finds no byte behind it, so an Esc
        // written while `x` is still in the buffer arrives as Alt+x.
        let mut child = child;
        let stalled = [
            ("mira-pty-ready", &b"\x1b[B"[..]),
            ("mira-pty-down", b"x"),
            ("mira-pty-x", b"\x1b"),
        ]
        .into_iter()
        .find(|(marker, keys)| !arrived(marker) || wr.write_all(keys).is_err())
        .map(|(marker, _)| marker);
        if stalled.is_some() {
            // Otherwise it sits in `poll_in(-1)` for the rest of the afternoon.
            let _ = child.kill();
        }

        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        let screen = screen.lock().unwrap().clone();
        // The child signs off by panicking on purpose, so any *other* panic is a
        // failed assertion and this is where it gets read out.
        assert!(
            stalled.is_none() && err.contains("mira-pty-done"),
            "child stalled at {stalled:?}: {err}\nscreen:\n{screen:?}"
        );
        // The frame reached the terminal, cursor-homed and erased behind itself.
        assert!(screen.contains("\x1b[H"), "{screen:?}");
        assert!(screen.contains("mira-pty-ready\x1b[K\r\n"), "{screen:?}");
        // ...and the panic hook handed the terminal back: cursor on, alternate
        // screen off. Without it the user's shell is left in raw mode.
        assert!(screen.contains("\x1b[?25h\x1b[?1049l"), "{screen:?}");
    }

    /// The child of the test above. Panics are the failure channel: they land on
    /// the piped stderr and the exit status carries them back.
    fn on_the_pty() {
        let mut t = Term::enter().expect("stdin and stdout are the pty slave");
        assert_eq!(t.size(), (120, 40), "the size the parent set");

        // Before the first marker, so the parent provably has not typed yet: the
        // timeout expires and says so, rather than returning a key nobody
        // pressed. After a marker it would be a race the parent usually wins.
        assert_eq!(t.key(30).unwrap(), None, "empty poll");

        // A resize has to break an *infinite* poll, or the frame is never
        // repainted at the new size. Aimed at this exact thread: plain `kill`
        // is free to hand the signal to the test harness's thread, which is not
        // the one parked in `poll`.
        struct Tid(libc::pthread_t);
        // Handing a thread handle to the thread that will signal with it is the
        // only reason this type exists.
        // SAFETY: `pthread_t` is a pointer on macOS, which is the only reason
        // `Tid` is not `Send` already. The receiving thread does one thing with
        // it — `pthread_kill`, which POSIX requires to be callable from any
        // thread — and the handle cannot dangle, because this thread is parked
        // in the `t.key(-1)` below until the signal it sends arrives. Delete
        // that call and the id can outlive its thread.
        unsafe impl Send for Tid {}
        // SAFETY: `pthread_self` reads no memory and cannot fail.
        let me = Tid(unsafe { libc::pthread_self() });
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            // SAFETY: `me.0` names the thread that is blocked in `poll` waiting
            // for this — see the `unsafe impl` above for why it is still alive.
            // SIGWINCH has a handler by now (`Term::enter` installed it), so
            // delivery interrupts that poll instead of killing the process.
            unsafe { libc::pthread_kill(me.0, libc::SIGWINCH) };
        });
        assert_eq!(t.key(-1).unwrap(), None, "a resize interrupts the poll");

        // SIGTERM and SIGHUP are inspected rather than raised: they end the
        // process, and a child that dies of a signal writes no coverage profile
        // and reports nothing back. The failure that actually happened — no
        // handler at all, so a `kill` left the shell in raw mode — is visible
        // from the disposition. `SA_RESTART` is not asserted on: see
        // [`on_signal`] for why the flag has no bearing on either syscall in
        // this loop, and the wake-up above for the behaviour that does matter.
        for sig in [libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: POD and all-zero is a valid `sigaction`, as in
            // [`on_signal`]; here it is only a destination.
            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: a null `act` means "report the disposition without
            // changing it", so the only pointer that has to be good is `&mut
            // sa`, a live and correctly typed `sigaction`.
            let got = unsafe { libc::sigaction(sig, std::ptr::null(), &mut sa) };
            assert_eq!(got, 0, "reading the disposition of {sig}");
            assert!(
                sa.sa_sigaction != libc::SIG_DFL && sa.sa_sigaction != libc::SIG_IGN,
                "signal {sig} has no handler"
            );
        }

        // Each marker is both a frame to assert on and the parent's cue to send
        // the next key. See the write loop above for why they interleave.
        t.draw(&["mira-pty-ready".into(), "second row".into()])
            .unwrap();
        assert_eq!(t.key(-1).unwrap(), Some(Key::Down));
        t.draw(&["mira-pty-down".into()]).unwrap();
        assert_eq!(t.key(-1).unwrap(), Some(Key::Char('x')));
        t.draw(&["mira-pty-x".into()]).unwrap();
        // The lone Esc, which only resolves because the grace period ran out.
        assert_eq!(t.key(-1).unwrap(), Some(Key::Esc));

        // A panic, on purpose, as the last act: the promise `enter` makes by
        // installing a hook is that a crash still gives you your terminal back,
        // and the only way to check it is to crash. The parent looks for this
        // exact message, so a real assertion failure above still reads as one.
        panic!("mira-pty-done");
    }
}
