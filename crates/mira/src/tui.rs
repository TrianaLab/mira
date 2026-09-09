//! The terminal UI.
//!
//! Same four questions the browser UI asks, over the same three routes, in the
//! place the person asking them already is. The browser UI wins on waterfalls
//! and charts; this wins on being one `kubectl exec` away and on working with
//! no server running at all — [`Source::Local`] reads a block directory
//! in-process, so a detached volume is still readable after the pod that wrote
//! it is gone.
//!
//! Everything here is synchronous. There is no runtime, no task and no channel:
//! the loop draws, blocks on a key, and runs one query. A query that takes two
//! seconds freezes the UI for two seconds — which is why the frame is painted
//! *before* the query runs, so the freeze always has a "running" on it rather
//! than a stale screen.

mod source;

use std::time::Instant;

use yaml_rust2::Yaml;

use crate::term::{self, Key, Row, Term};
use mira_core::json::Json;

pub use source::{Source, parse_addr};

/// Selectable query windows, coarse on purpose: the point of `[` and `]` is to
/// change the answer in one keystroke, and a continuous control would need two.
const WINDOWS: [&str; 7] = ["5m", "15m", "1h", "6h", "24h", "7d", "30d"];

pub fn run(src: Source) -> Result<(), String> {
    let mut app = App::new(src);
    let mut term = Term::enter().map_err(|e| e.to_string())?;
    loop {
        let (w, h) = term.size();
        term.draw(&app.frame(w, h)).map_err(|e| e.to_string())?;

        // Deferred so the frame above — the one that says what is running — is
        // on screen before the query blocks the thread that would have drawn it.
        if let Some(job) = app.job.take() {
            app.run(job, h);
            continue;
        }
        match term.key(-1).map_err(|e| e.to_string())? {
            Some(k) if !app.key(k, h) => return Ok(()),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Logs,
    Traces,
    Metrics,
}

impl Tab {
    fn signal(self) -> &'static str {
        match self {
            Tab::Traces => "traces",
            _ => "logs",
        }
    }

    /// Where a filter word with no operator goes.
    ///
    /// Someone who types `refused` means "show me the ones that say refused",
    /// which is what every log viewer in the market does with a bare word and
    /// what this one used to do with it: nothing at all, silently, while the
    /// filter bar showed the word and the rows looked filtered.
    ///
    /// `None` for metrics, whose `where` terms are attribute predicates on data
    /// points — there is no text column to search, so a bare word there is
    /// reported rather than invented a meaning for.
    fn free_text(self) -> Option<&'static str> {
        match self {
            Tab::Logs => Some("body"),
            Tab::Traces => Some("name"),
            Tab::Metrics => None,
        }
    }

    /// Columns of the signal's root table.
    ///
    /// This is what decides whether `name=checkout` filters a column or an
    /// attribute, and getting it wrong is silent both ways: an unknown `field`
    /// matches nothing rather than erroring, and a column missing from this
    /// list is sent as `attr`, which the Bloom filter prunes to zero rows. That
    /// is what happened to `event_name`, added to the schema after this list
    /// was written — so `tests::the_field_list_is_the_schema` now pins the two
    /// together. Not an intra-doc link: the target is behind `cfg(test)`, so it
    /// does not exist in the configuration rustdoc builds.
    fn fields(self) -> &'static [&'static str] {
        match self {
            Tab::Traces => &[
                "trace_id",
                "span_id",
                "parent_span_id",
                "trace_state",
                "flags",
                "name",
                "kind",
                "start_time_unix_nano",
                "duration_nano",
                "status_code",
                "status_message",
                "dropped_attributes_count",
                "dropped_events_count",
                "dropped_links_count",
            ],
            _ => &[
                "time_unix_nano",
                "observed_time_unix_nano",
                "severity_number",
                "severity_text",
                "event_name",
                "body",
                "trace_id",
                "span_id",
                "flags",
                "dropped_attributes_count",
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Filter,
    Detail,
    Trace,
    /// One span of the waterfall, in the same renderer [`Mode::Detail`] uses.
    ///
    /// A separate mode rather than a flag on `Detail` because the two differ in
    /// where they came from and where Esc goes back to, and because the span is
    /// not in `self.rows` — see [`App::selected`].
    Span,
    Help,
}

enum Job {
    Rows,
    Names,
    Series,
    Trace(String),
}

struct App {
    src: Source,
    tab: Tab,
    win: usize,
    limit: usize,
    filter: String,
    /// The filter as it was before `/` was pressed, so Esc can put it back.
    filter_undo: String,
    mode: Mode,

    rows: Vec<Yaml>,
    sel: usize,

    names: Vec<Yaml>,
    nsel: usize,
    series: Vec<Yaml>,
    ssel: usize,
    /// Which metrics pane the arrow keys drive.
    on_series: bool,

    trace: Option<Trace>,
    scroll: usize,
    stats: String,
    status: String,
    err: bool,
    job: Option<Job>,
}

struct Trace {
    id: String,
    /// Spans in waterfall order — depth-first from each root, children by start
    /// time — paired with their indent depth.
    spans: Vec<(Yaml, usize)>,
    t0: i64,
    span_ns: i64,
    sel: usize,
}

impl App {
    fn new(src: Source) -> App {
        App {
            src,
            tab: Tab::Logs,
            win: 2,
            limit: 200,
            filter: String::new(),
            filter_undo: String::new(),
            mode: Mode::List,
            rows: Vec::new(),
            sel: 0,
            names: Vec::new(),
            nsel: 0,
            series: Vec::new(),
            ssel: 0,
            on_series: false,
            trace: None,
            scroll: 0,
            stats: String::new(),
            status: "loading".into(),
            err: false,
            job: Some(Job::Rows),
        }
    }

    // ---- queries ----------------------------------------------------------

    fn reload(&mut self) {
        self.status = "running".into();
        self.err = false;
        self.job = Some(match self.tab {
            Tab::Metrics => Job::Names,
            _ => Job::Rows,
        });
    }

    fn run(&mut self, job: Job, h: usize) {
        let t = Instant::now();
        let r = match &job {
            Job::Rows => self.src.post(source::QUERY, &self.rows_query()),
            Job::Names => self.src.post(source::NAMES, &self.window_query()),
            Job::Series => self.src.post(source::SERIES, &self.series_query()),
            Job::Trace(id) => self.src.post(source::QUERY, &trace_query(id)),
        };
        let doc = match r {
            Ok(d) => d,
            Err(e) => {
                self.status = e;
                self.err = true;
                return;
            }
        };
        self.err = false;
        self.stats = format!("{} · {}", stats_line(&doc["stats"]), ms(t.elapsed()));
        self.status.clear();

        if let Some(w) = self.ignored_word() {
            self.status = format!("ignored {w:?}: metrics filters are attr=value terms");
        }

        match job {
            Job::Rows => {
                self.rows = array(&doc["rows"]);
                self.sel = 0;
                self.scroll = 0;
                if self.rows.is_empty() {
                    self.status = "no rows in this window".into();
                }
            }
            Job::Names => {
                self.names = array(&doc["names"]);
                self.nsel = 0;
                self.on_series = false;
                match self.names.is_empty() {
                    true => self.status = "no metrics in this window".into(),
                    // One name selected is one chart the user did not have to
                    // ask for; the metrics tab is useless until a series loads.
                    false => self.job = Some(Job::Series),
                }
            }
            Job::Series => {
                self.series = array(&doc["series"]);
                self.ssel = 0;
                self.scroll = 0;
            }
            Job::Trace(id) => {
                let spans = array(&doc["rows"]);
                if spans.is_empty() {
                    self.status = format!("no spans found for trace {id}");
                    self.err = true;
                    return;
                }
                self.trace = Some(Trace::new(id, &spans));
                self.mode = Mode::Trace;
                self.scroll = 0;
                let _ = h;
            }
        }
    }

    fn rows_query(&self) -> String {
        let mut j = Json::new();
        j.obj(|j| {
            j.key("signal");
            j.str(self.tab.signal());
            j.key("from");
            j.str(&format!("-{}", WINDOWS[self.win]));
            j.key("to");
            j.str("now");
            j.key("limit");
            j.i64(self.limit as i64);
            j.key("where");
            self.terms(j);
        });
        j.into_string()
    }

    fn window_query(&self) -> String {
        let mut j = Json::new();
        j.obj(|j| {
            j.key("from");
            j.str(&format!("-{}", WINDOWS[self.win]));
            j.key("to");
            j.str("now");
        });
        j.into_string()
    }

    fn series_query(&self) -> String {
        let name = self
            .names
            .get(self.nsel)
            .and_then(|n| n["name"].as_str())
            .unwrap_or_default()
            .to_owned();
        let mut j = Json::new();
        j.obj(|j| {
            j.key("name");
            j.str(&name);
            j.key("from");
            j.str(&format!("-{}", WINDOWS[self.win]));
            j.key("to");
            j.str("now");
            // ponytail: 64 series is three screenfuls at three lines each, and
            // the engine does not report `max_series` truncation the way it
            // reports `max_points`, so the 65th is invisible here. Give it a
            // badge when the response grows a count to put in one.
            j.key("max_series");
            j.i64(64);
            // A sparkline is one character per point, so the API's default of
            // 5 000 would download three orders of magnitude more than the
            // widest terminal can render. Not scaled to the terminal either:
            // `spark` buckets by the column's peak, so points past the column
            // count still decide whether a spike shows, and 400 is enough of
            // them for any width. What the cap costs — it keeps the newest
            // 400, not a thinned 400 — is on screen as `+n dropped`.
            j.key("max_points");
            j.i64(400);
            j.key("where");
            self.terms(j);
        });
        j.into_string()
    }

    /// A word in the filter that this tab has nowhere to put.
    ///
    /// Only metrics can produce one — every other tab reads a bare word as free
    /// text. The one thing that must not happen is silence: the filter bar goes
    /// on showing the word, so the rows look filtered when they are not.
    fn ignored_word(&self) -> Option<String> {
        if self.tab.free_text().is_some() {
            return None;
        }
        parse_filter(&self.filter)
            .into_iter()
            .find_map(|p| match p {
                Part::Word(w) => Some(w),
                Part::Term(..) => None,
            })
    }

    fn terms(&self, j: &mut Json) {
        j.arr(|j| {
            for part in parse_filter(&self.filter) {
                let (key, op, val) = match part {
                    Part::Term(k, op, v) => (k, op, v),
                    // A bare word is free text over the tab's message column.
                    // Quoted, so `scalar` cannot decide that `500` was a number
                    // and hand `body` an integer to compare against.
                    Part::Word(w) => match self.tab.free_text() {
                        Some(f) => {
                            j.obj(|j| {
                                j.key("field");
                                j.str(f);
                                j.key("contains");
                                j.str(&w);
                            });
                            continue;
                        }
                        None => continue,
                    },
                };
                let field = self.tab.fields().contains(&key.as_str());
                j.obj(|j| {
                    j.key(if field { "field" } else { "attr" });
                    j.str(&key);
                    j.key(op);
                    scalar(j, &key, &val);
                });
            }
        });
    }

    // ---- keys -------------------------------------------------------------

    /// Handle one key. `false` means quit.
    fn key(&mut self, k: Key, h: usize) -> bool {
        if self.mode == Mode::Filter {
            return self.filter_key(k);
        }
        let page = body_h(h).saturating_sub(1).max(1);
        match k {
            Key::Char('q') | Key::Ctrl('c') if self.mode == Mode::List => return false,
            // Back one step, not back to the list: a span detail was opened
            // from the waterfall and that is where its reader still is.
            Key::Char('q') | Key::Esc => {
                self.mode = match self.mode {
                    Mode::Span => Mode::Trace,
                    _ => Mode::List,
                }
            }
            Key::Ctrl('c') => return false,
            Key::Char('?') => {
                self.mode = match self.mode {
                    Mode::Help => Mode::List,
                    _ => Mode::Help,
                }
            }
            Key::Char('1') => self.go(Tab::Logs),
            Key::Char('2') => self.go(Tab::Traces),
            Key::Char('3') => self.go(Tab::Metrics),
            Key::Char('h') | Key::Left => self.go(match self.tab {
                Tab::Logs => Tab::Metrics,
                Tab::Traces => Tab::Logs,
                Tab::Metrics => Tab::Traces,
            }),
            Key::Char('l') | Key::Right => self.go(match self.tab {
                Tab::Logs => Tab::Traces,
                Tab::Traces => Tab::Metrics,
                Tab::Metrics => Tab::Logs,
            }),
            Key::Tab if self.tab == Tab::Metrics && self.mode == Mode::List => {
                self.on_series = !self.on_series;
            }
            Key::Char('j') | Key::Down => self.move_by(1),
            Key::Char('k') | Key::Up => self.move_by(-1),
            Key::PageDown | Key::Ctrl('f') => self.move_by(page as isize),
            Key::PageUp | Key::Ctrl('b') => self.move_by(-(page as isize)),
            Key::Home | Key::Char('g') => self.move_to(0),
            Key::End | Key::Char('G') => self.move_to(usize::MAX),
            Key::Char('/') if self.mode == Mode::List => {
                self.filter_undo = self.filter.clone();
                self.mode = Mode::Filter;
            }
            Key::Char('r') => self.reload(),
            Key::Char('[') => {
                self.win = self.win.saturating_sub(1);
                self.reload();
            }
            Key::Char(']') => {
                self.win = (self.win + 1).min(WINDOWS.len() - 1);
                self.reload();
            }
            Key::Char('+') | Key::Char('=') => {
                self.limit = (self.limit * 2).min(10_000);
                self.reload();
            }
            Key::Char('-') => {
                self.limit = (self.limit / 2).max(10);
                self.reload();
            }
            Key::Enter => match self.mode {
                Mode::List if self.tab == Tab::Metrics && !self.on_series => {
                    self.job = Some(Job::Series);
                    self.status = "running".into();
                }
                Mode::List => {
                    self.mode = Mode::Detail;
                    self.scroll = 0;
                }
                // The waterfall renders a span's shape; its attributes and its
                // status message only exist in the detail view, and without
                // this the selected span had no way to reach one.
                Mode::Trace => {
                    self.mode = Mode::Span;
                    self.scroll = 0;
                }
                _ => {}
            },
            Key::Char('t') => self.open_trace(),
            _ => {}
        }
        true
    }

    fn filter_key(&mut self, k: Key) -> bool {
        match k {
            Key::Enter => {
                self.mode = Mode::List;
                self.reload();
            }
            Key::Esc | Key::Ctrl('c') => {
                self.filter = std::mem::take(&mut self.filter_undo);
                self.mode = Mode::List;
            }
            Key::Backspace => {
                self.filter.pop();
            }
            Key::Ctrl('u') => self.filter.clear(),
            Key::Ctrl('w') => {
                let keep = self.filter.trim_end();
                let cut = keep.rfind(' ').map_or(0, |i| i + 1);
                self.filter.truncate(cut);
            }
            Key::Char(c) => self.filter.push(c),
            _ => {}
        }
        true
    }

    fn go(&mut self, tab: Tab) {
        if self.tab != tab || self.mode != Mode::List {
            self.tab = tab;
            self.mode = Mode::List;
            self.reload();
        }
    }

    /// Move whichever cursor the current pane owns.
    ///
    /// One function rather than one per view because every view's list is a
    /// selected index plus a length, and the clamping is the part that is easy
    /// to get wrong twice.
    ///
    /// Clamped in `usize`, not `isize`. The scrolling panes report their length
    /// as `usize::MAX` — see [`cursor`](App::cursor) — and that is a negative
    /// `isize`, so an `isize` clamp reads its own upper bound as below its lower
    /// bound and panics. `j` in the detail pane is the keystroke that did it.
    fn move_by(&mut self, d: isize) {
        let (sel, len) = self.cursor();
        self.set_cursor(sel.saturating_add_signed(d).min(len.saturating_sub(1)));
    }

    fn move_to(&mut self, n: usize) {
        let (_, len) = self.cursor();
        self.set_cursor(n.min(len.saturating_sub(1)));
    }

    fn cursor(&self) -> (usize, usize) {
        match (self.mode, self.tab) {
            // The detail and help panes scroll rather than select, but the
            // arithmetic is the same and the bound is the line count, which the
            // renderer knows and this does not — so let it run to the end and
            // let `frame` clamp.
            (Mode::Detail | Mode::Span | Mode::Help, _) => (self.scroll, usize::MAX),
            (Mode::Trace, _) => (
                self.trace.as_ref().map_or(0, |t| t.sel),
                self.trace.as_ref().map_or(0, |t| t.spans.len()),
            ),
            (_, Tab::Metrics) if self.on_series => (self.ssel, self.series.len()),
            (_, Tab::Metrics) => (self.nsel, self.names.len()),
            _ => (self.sel, self.rows.len()),
        }
    }

    fn set_cursor(&mut self, n: usize) {
        match (self.mode, self.tab) {
            (Mode::Detail | Mode::Span | Mode::Help, _) => self.scroll = n,
            (Mode::Trace, _) => {
                if let Some(t) = self.trace.as_mut() {
                    t.sel = n;
                }
            }
            (_, Tab::Metrics) if self.on_series => self.ssel = n,
            (_, Tab::Metrics) => self.nsel = n,
            _ => self.sel = n,
        }
    }

    /// Follow whatever trace the selection points at.
    ///
    /// This is the correlation story as one keystroke: a log line carries the
    /// `trace_id` of the request that emitted it, a span carries its own, and a
    /// metric exemplar carries the id of the request that produced the
    /// measurement. Three different tabs, one key, because to the person
    /// looking it is the same question.
    fn open_trace(&mut self) {
        let id = match (self.mode, self.tab) {
            // Already inside the trace this would open.
            (Mode::Trace | Mode::Span, _) => return,
            (_, Tab::Metrics) => self
                .series
                .get(self.ssel)
                .and_then(|s| s["exemplars"][0]["trace_id"].as_str())
                .map(str::to_owned),
            _ => self
                .rows
                .get(self.sel)
                .and_then(|r| r["trace_id"].as_str())
                .map(str::to_owned),
        };
        match id.filter(|s| s.len() == 32 && s.bytes().any(|b| b != b'0')) {
            Some(id) => {
                self.status = format!("loading trace {id}");
                self.job = Some(Job::Trace(id));
            }
            None => {
                self.status = "nothing here carries a trace id".into();
                self.err = true;
            }
        }
    }

    fn selected(&self) -> Option<&Yaml> {
        match (self.mode, self.tab) {
            // The waterfall's own selection, which is not in `self.rows` and
            // usually cannot be: a followed trace is queried over all of
            // retention, so its spans are rarely the rows the list tab holds.
            (Mode::Span, _) => self
                .trace
                .as_ref()
                .and_then(|t| t.spans.get(t.sel))
                .map(|(s, _)| s),
            (_, Tab::Metrics) => self.series.get(self.ssel),
            _ => self.rows.get(self.sel),
        }
    }

    // ---- rendering --------------------------------------------------------

    fn frame(&mut self, w: usize, h: usize) -> Vec<String> {
        // A terminal narrower than this cannot show a timestamp and a body, and
        // every column computation below starts clamping to zero. Drawing the
        // frame at 40 anyway is worse than not drawing it: `Term::draw` has no
        // cursor addressing, so every over-wide row wraps and the top of the
        // frame scrolls off for good, with nothing on screen saying why.
        if w < 40 || h < 8 {
            let mut r = Row::new(w);
            r.put(term::RED, "terminal too small — need 40x8");
            return vec![r.done()];
        }
        let mut out = Vec::with_capacity(h);
        out.push(self.tabbar(w));
        out.push(self.filterbar(w));

        let bh = body_h(h);
        let mut body = match self.mode {
            Mode::Help => help(w),
            Mode::Detail | Mode::Span => self.detail_full(w),
            Mode::Trace => self.waterfall(w, bh),
            _ => match self.tab {
                Tab::Metrics => self.metrics(w, bh),
                _ => self.records(w, bh),
            },
        };
        // Scrolling panes hand back every line they have and are windowed here,
        // so each one does not have to reimplement the clamp.
        if matches!(self.mode, Mode::Detail | Mode::Span | Mode::Help) {
            self.scroll = self.scroll.min(body.len().saturating_sub(1));
            body = body.into_iter().skip(self.scroll).take(bh).collect();
        }
        body.truncate(bh);
        while body.len() < bh {
            body.push(String::new());
        }
        out.extend(body);
        out.push(self.statusbar(w));
        out.push(self.hints(w));
        out
    }

    fn tabbar(&self, w: usize) -> String {
        let mut r = Row::new(w);
        r.put(term::BOLD, " mira ");
        for (i, (n, t)) in [
            ("1 logs", Tab::Logs),
            ("2 traces", Tab::Traces),
            ("3 metrics", Tab::Metrics),
        ]
        .iter()
        .enumerate()
        {
            r.plain(if i == 0 { " " } else { "  " });
            match *t == self.tab {
                true => r.put(term::REV, &format!(" {n} ")),
                false => r.put(term::DIM, &format!(" {n} ")),
            };
        }
        let label = self.src.label();
        r.pad_to(w.saturating_sub(label.len() + 1));
        r.put(term::DIM, &label).plain(" ");
        r.done()
    }

    fn filterbar(&self, w: usize) -> String {
        // The window and limit are reserved before anything else is written, so
        // an 80-column terminal clips the filter rather than the two fields that
        // say what the filter was applied to.
        let right = format!("last {}  limit {}", WINDOWS[self.win], self.limit);
        let keep = w.saturating_sub(right.len() + 2);
        let mut r = Row::new(keep);
        let editing = self.mode == Mode::Filter;
        r.put(
            if editing { term::BOLD } else { term::DIM },
            if editing { " filter> " } else { " filter  " },
        );
        match (self.filter.is_empty(), editing) {
            (true, false) => {
                r.put(
                    term::DIM,
                    "(none) — press / to add one, e.g. service.name=checkout",
                );
            }
            _ => {
                r.plain(&self.filter);
                if editing {
                    // A block where the cursor would be. The real cursor is
                    // hidden for the whole session, so drawing one is cheaper
                    // than showing and positioning it every frame.
                    r.put(term::REV, " ");
                }
            }
        }
        r.cap(w).pad_to(w.saturating_sub(right.len() + 1));
        r.put(term::DIM, &right).plain(" ");
        r.done()
    }

    fn statusbar(&self, w: usize) -> String {
        let mut r = Row::new(w);
        r.plain(" ");
        match self.status.is_empty() {
            false => {
                r.put(
                    if self.err { term::RED } else { term::YELLOW },
                    &self.status,
                );
            }
            true => {
                r.put(term::DIM, &self.stats);
            }
        }
        if !self.status.is_empty() && !self.stats.is_empty() {
            r.plain("  ");
            r.put(term::DIM, &self.stats);
        }
        r.done()
    }

    fn hints(&self, w: usize) -> String {
        let keys = match self.mode {
            Mode::Filter => "enter apply  esc cancel  ^w word  ^u clear",
            Mode::Detail => "esc back  ↑↓ scroll  t trace",
            Mode::Trace => "esc back  ↑↓ span  enter detail",
            Mode::Span => "esc waterfall  ↑↓ scroll",
            Mode::Help => "esc back",
            Mode::List if self.tab == Tab::Metrics => {
                "↑↓ move  tab pane  enter load  t trace  / filter  [] window  r reload  ? help  q quit"
            }
            Mode::List => {
                "↑↓ move  enter detail  t trace  / filter  [] window  +- limit  r reload  ? help  q quit"
            }
        };
        let mut r = Row::new(w);
        r.put(term::DIM, &format!(" {keys}"));
        r.done()
    }

    /// Logs and traces: a list on top, the selection's detail below.
    fn records(&self, w: usize, h: usize) -> Vec<String> {
        // Two thirds to the list. Below about a quarter the detail pane shows
        // nothing useful, and above about a half the list stops being a list.
        let list_h = (h * 2 / 3).max(1);
        let top = window_start(self.sel, list_h, self.rows.len());
        let scale = match self.tab {
            Tab::Traces => self
                .rows
                .iter()
                .filter_map(|r| r["duration_nano"].as_i64())
                .max()
                .unwrap_or(1)
                .max(1),
            _ => 1,
        };

        let mut out: Vec<String> = self
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(list_h)
            .map(|(i, row)| match self.tab {
                Tab::Traces => span_row(row, w, i == self.sel, scale),
                _ => log_row(row, w, i == self.sel),
            })
            .collect();
        while out.len() < list_h {
            out.push(String::new());
        }

        let title = match self.selected() {
            Some(_) => format!(
                " {} {} of {} ",
                match self.tab {
                    Tab::Traces => "span",
                    _ => "record",
                },
                self.sel + 1,
                self.rows.len()
            ),
            None => " nothing selected ".into(),
        };
        out.push(rule(w, &title));
        let left = h - out.len();
        if let Some(row) = self.selected() {
            out.extend(detail(row, w).into_iter().take(left));
        }
        out
    }

    fn detail_full(&self, w: usize) -> Vec<String> {
        match self.selected() {
            Some(row) => detail(row, w),
            None => {
                let mut r = Row::new(w);
                r.put(term::DIM, "  nothing selected");
                vec![r.done()]
            }
        }
    }

    /// Metric names on the left, the selected name's series on the right.
    fn metrics(&self, w: usize, h: usize) -> Vec<String> {
        let nw = 34.min(w / 3);
        let top = window_start(self.nsel, h, self.names.len());
        let mut out = Vec::with_capacity(h);

        let chart_w = w.saturating_sub(nw + 3);
        let right = self.series_lines(chart_w);
        let rtop = window_start(self.ssel * 3, h, right.len());

        for i in 0..h {
            let mut r = Row::new(w);
            if let Some(n) = self.names.get(top + i) {
                let sel = top + i == self.nsel;
                let style = match (sel, self.on_series) {
                    (true, false) => term::REV,
                    (true, true) => term::BOLD,
                    _ => "",
                };
                let name = clip(n["name"].as_str().unwrap_or("?"), nw.saturating_sub(9));
                r.put(style, &format!(" {name}"));
                r.pad_to(nw.saturating_sub(8));
                r.put(term::DIM, &clip(n["kind"].as_str().unwrap_or(""), 8));
            }
            r.pad_to(nw);
            r.put(term::DIM, " │ ");
            if let Some(line) = right.get(rtop + i) {
                // Already styled and already padded to `chart_w` by `done`.
                r.raw(line, chart_w);
            }
            out.push(r.done());
        }
        out
    }

    /// Three lines per series: its attributes, its sparkline, its exemplars.
    ///
    /// Pre-rendered as plain strings and then windowed by the caller, because
    /// the vertical scroll is over series and each one is a fixed three rows.
    fn series_lines(&self, w: usize) -> Vec<String> {
        if self.series.is_empty() {
            // Through a `Row` even when it is one word, so the caller's promise
            // that every line here is exactly `w` wide holds for this one too.
            let mut r = Row::new(w);
            if !self.names.is_empty() {
                r.put(term::DIM, " enter to load this metric");
            }
            return vec![r.done()];
        }
        let mut out = Vec::with_capacity(self.series.len() * 3);
        for (i, s) in self.series.iter().enumerate() {
            let sel = self.on_series && i == self.ssel;
            let mark = if sel { "▌" } else { " " };
            let attrs = pairs(&s["attributes"])
                .iter()
                .filter(|(k, _)| !k.starts_with("otel.scope."))
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            let mut r = Row::new(w);
            r.plain(mark);
            r.put(if sel { term::BOLD } else { "" }, &attrs);
            out.push(r.done());

            let pts: Vec<f64> = s["points"]
                .as_vec()
                .map(|v| v.iter().filter_map(|p| num(&p[1])).collect())
                .unwrap_or_default();
            let mut r = Row::new(w);
            r.plain(mark);
            match pts.is_empty() {
                true => {
                    r.put(term::DIM, "no points");
                }
                false => {
                    let (lo, hi) = (
                        pts.iter().cloned().fold(f64::INFINITY, f64::min),
                        pts.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    );
                    // `max_points` truncation is newest-wins, so a capped
                    // sparkline is the tail of the window drawn under a filter
                    // bar that still says `last 24h`. Its width is taken out of
                    // the bar rather than appended, because `Row` clips at the
                    // edge and the one thing that must not be clipped is the
                    // line saying the picture is incomplete. The browser legend
                    // carries the same badge, for the same reason.
                    let badge = match s["dropped_points"].as_i64().unwrap_or(0) {
                        0 => String::new(),
                        n => format!("  +{n} dropped"),
                    };
                    let bars = w.saturating_sub(28 + badge.len()).max(8);
                    r.put(term::CYAN, &spark(&pts, bars));
                    r.plain("  ");
                    r.put(term::DIM, &format!("{} → {}", g(lo), g(hi)));
                    if !badge.is_empty() {
                        r.put(term::YELLOW, &badge);
                    }
                }
            }
            let ex = s["exemplars"].as_vec().map_or(0, Vec::len);
            if ex > 0 {
                r.plain(" ");
                r.put(term::MAGENTA, &format!("◆{ex}"));
            }
            out.push(r.done());

            let mut r = Row::new(w);
            r.plain(mark);
            if let Some(id) = s["exemplars"][0]["trace_id"].as_str() {
                r.put(term::MAGENTA, "◆ ");
                r.put(term::DIM, "trace ");
                r.plain(&clip(id, 32));
                r.put(term::DIM, "  t to open");
            }
            out.push(r.done());
        }
        out
    }

    fn waterfall(&self, w: usize, h: usize) -> Vec<String> {
        let Some(t) = &self.trace else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(h);
        let mut head = Row::new(w);
        head.put(term::BOLD, " trace ").plain(&t.id);
        head.put(
            term::DIM,
            &format!("  ·  {} spans  ·  {}", t.spans.len(), dur(t.span_ns.max(0))),
        );
        out.push(head.done());
        out.push(rule(w, ""));

        // Where each column ends: the tree, the service, the bar, the duration.
        // Ends rather than widths because every write is "fill up to here", and
        // the last one is `w`, so the row always adds up to the frame.
        let namew = (w / 3).clamp(16, 46);
        let svcw = (w / 6).clamp(8, 20);
        // `run` never draws below 40 columns, which leaves the bar five cells
        // once the name, service and duration have taken theirs.
        let ends = [namew, namew + svcw, w.saturating_sub(11), w];

        let lines: Vec<String> = t
            .spans
            .iter()
            .enumerate()
            .flat_map(|(i, (s, depth))| {
                let mut rows = vec![span_bar(s, *depth, i == t.sel, ends, t)];
                for e in s["events"].as_vec().into_iter().flatten() {
                    let mut r = Row::new(w);
                    let at = e["time_unix_nano"].as_i64().unwrap_or(t.t0) - t.t0;
                    r.plain(&" ".repeat((depth + 2).min(namew)));
                    r.put(term::YELLOW, "● ");
                    r.plain(e["name"].as_str().unwrap_or("event"));
                    r.put(term::DIM, &format!("  +{}", dur(at.max(0))));
                    rows.push(r.done());
                }
                for l in s["links"].as_vec().into_iter().flatten() {
                    let mut r = Row::new(w);
                    r.plain(&" ".repeat((depth + 2).min(namew)));
                    r.put(term::BLUE, "↗ ");
                    r.put(term::DIM, "trace ");
                    r.plain(l["trace_id"].as_str().unwrap_or("?"));
                    rows.push(r.done());
                }
                rows
            })
            .collect();

        // Scroll so the selected span stays on screen. Its line number is not
        // its index — events and links push it down — so it is counted rather
        // than assumed.
        let selected_line = t
            .spans
            .iter()
            .take(t.sel)
            .map(|(s, _)| {
                1 + s["events"].as_vec().map_or(0, Vec::len)
                    + s["links"].as_vec().map_or(0, Vec::len)
            })
            .sum::<usize>();
        let vis = h.saturating_sub(2);
        let top = window_start(selected_line, vis, lines.len());
        out.extend(lines.into_iter().skip(top).take(vis));
        out
    }
}

impl Trace {
    fn new(id: String, spans: &[Yaml]) -> Trace {
        let t0 = spans
            .iter()
            .filter_map(|s| s["start_time_unix_nano"].as_i64())
            .min()
            .unwrap_or(0);
        let t1 = spans
            .iter()
            .filter_map(|s| {
                Some(s["start_time_unix_nano"].as_i64()? + s["duration_nano"].as_i64().unwrap_or(0))
            })
            .max()
            .unwrap_or(t0 + 1);

        // Depth-first from each root, children ordered by start time — the
        // order a waterfall is read in. A span whose parent is not in the set
        // (sampled away, or emitted by a service whose data went elsewhere) is
        // a root too, otherwise it would be dropped from a view whose whole job
        // is to show everything about one request.
        let ids: std::collections::HashSet<&str> =
            spans.iter().filter_map(|s| s["span_id"].as_str()).collect();
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by_key(|&i| spans[i]["start_time_unix_nano"].as_i64().unwrap_or(0));

        let mut out = Vec::with_capacity(spans.len());
        let mut stack: Vec<(usize, usize)> = order
            .iter()
            .rev()
            .filter(|&&i| {
                !spans[i]["parent_span_id"]
                    .as_str()
                    .is_some_and(|p| ids.contains(p))
            })
            .map(|&i| (i, 0))
            .collect();
        while let Some((i, depth)) = stack.pop() {
            out.push((spans[i].clone(), depth));
            let me = spans[i]["span_id"].as_str().unwrap_or("");
            stack.extend(
                order
                    .iter()
                    .rev()
                    .filter(|&&c| spans[c]["parent_span_id"].as_str() == Some(me) && c != i)
                    .map(|&c| (c, depth + 1)),
            );
        }
        // A parent cycle would drop spans rather than loop, but it must not
        // silently lose them: anything unvisited goes on the end flat.
        if out.len() < spans.len() {
            let seen: std::collections::HashSet<String> = out
                .iter()
                .filter_map(|(s, _)| s["span_id"].as_str().map(str::to_owned))
                .collect();
            for &i in &order {
                if !spans[i]["span_id"]
                    .as_str()
                    .is_some_and(|s| seen.contains(s))
                {
                    out.push((spans[i].clone(), 0));
                }
            }
        }

        Trace {
            id,
            spans: out,
            t0,
            span_ns: t1 - t0,
            sel: 0,
        }
    }
}

// ---- row renderers --------------------------------------------------------

fn log_row(row: &Yaml, w: usize, sel: bool) -> String {
    let style = if sel { term::REV } else { "" };
    let sev = row["severity_number"].as_i64().unwrap_or(0);
    let mut r = Row::new(w);
    r.put(
        style,
        &format!(" {} ", hms(row["time_unix_nano"].as_i64().unwrap_or(0))),
    );
    r.put(
        if sel { style } else { sev_style(sev) },
        &format!(
            "{:<6}",
            clip(row["severity_text"].as_str().unwrap_or("-"), 6)
        ),
    );
    r.plain(" ");
    r.put(
        if sel { style } else { term::DIM },
        &format!("{:<16}", clip(service(row), 16)),
    );
    r.plain(" ");
    r.put(style, row["body"].as_str().unwrap_or(""));
    r.fill(style)
}

fn span_row(row: &Yaml, w: usize, sel: bool, scale: i64) -> String {
    let style = if sel { term::REV } else { "" };
    let d = row["duration_nano"].as_i64().unwrap_or(0);
    let error = row["status_code"].as_i64() == Some(2);
    let mut r = Row::new(w);
    r.put(
        style,
        &format!(
            " {} ",
            hms(row["start_time_unix_nano"].as_i64().unwrap_or(0))
        ),
    );
    r.put(style, &format!("{:>9} ", dur(d)));
    r.put(
        if sel { style } else { term::DIM },
        &format!("{:<16}", clip(service(row), 16)),
    );
    r.plain(" ");
    let name_style = match (sel, error) {
        (true, _) => style,
        (_, true) => term::RED,
        _ => "",
    };
    r.put(
        name_style,
        &format!("{:<30}", clip(row["name"].as_str().unwrap_or(""), 30)),
    );
    if error {
        r.put(if sel { style } else { term::RED }, " ERROR");
    }
    // A bar against the widest span in the result, so the outliers in a page of
    // results are visible without opening any of them.
    let bar = (d as f64 / scale as f64 * r.left().saturating_sub(2) as f64) as usize;
    r.plain(" ");
    r.repeat(if sel { style } else { term::BLUE }, '▂', bar.max(1));
    r.fill(style)
}

#[allow(clippy::too_many_arguments)]
/// One span's row: name, service, bar, duration, each clipped to its column.
///
/// `ends` is the column boundaries from [`App::waterfall`]. Every write caps the
/// row at its own end before writing, so an over-long span name eats into its
/// own column and nothing else — without the cap it would push the bar right and
/// shove the duration off the screen.
fn span_bar(s: &Yaml, depth: usize, sel: bool, ends: [usize; 4], t: &Trace) -> String {
    let style = if sel { term::REV } else { "" };
    let start = s["start_time_unix_nano"].as_i64().unwrap_or(t.t0) - t.t0;
    let d = s["duration_nano"].as_i64().unwrap_or(0);
    let error = s["status_code"].as_i64() == Some(2);
    let text = match (sel, error) {
        (true, _) => style,
        (_, true) => term::RED,
        _ => "",
    };

    let mut r = Row::new(ends[0]);
    let indent = " ".repeat(depth.min(ends[0] / 2));
    r.put(
        text,
        &format!(" {indent}{}", s["name"].as_str().unwrap_or("")),
    );
    r.cap(ends[1]).pad_to(ends[0]);
    r.put(term::DIM, &format!(" {}", service(s)))
        .pad_to(ends[1]);

    // Both offset and length come from the same scale, so a zero-duration span
    // still gets one cell and lands in the right place rather than vanishing.
    let barw = ends[2] - ends[1];
    let scale = |ns: i64| (ns as f64 / t.span_ns.max(1) as f64 * barw as f64) as usize;
    let off = scale(start).min(barw.saturating_sub(1));
    let len = scale(d).clamp(1, barw - off);
    r.cap(ends[2]).repeat("", ' ', off);
    r.repeat(if text.is_empty() { term::GREEN } else { text }, '█', len);
    r.pad_to(ends[2]);

    let durw = ends[3] - ends[2] - 1;
    r.cap(ends[3]);
    r.put(term::DIM, &format!("{:>durw$} ", dur(d)));
    r.fill(style)
}

/// The selected record, field by field, then its attributes.
fn detail(row: &Yaml, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let line = |k: &str, v: &str, style: &str| {
        let mut r = Row::new(w);
        r.put(term::DIM, &format!("  {k:<26}"));
        r.put(style, v);
        r.done()
    };
    for (k, v) in pairs(row) {
        if k == "attributes" || k == "events" || k == "links" || k == "points" {
            continue;
        }
        let pretty = match k.as_str() {
            "time_unix_nano" | "observed_time_unix_nano" | "start_time_unix_nano" => {
                v.parse::<i64>().map(stamp).unwrap_or(v.clone())
            }
            "duration_nano" => v.parse::<i64>().map(dur).unwrap_or(v.clone()),
            "kind" => v
                .parse::<i64>()
                .map(|k| kind(k).to_owned())
                .unwrap_or(v.clone()),
            _ => v.clone(),
        };
        out.push(line(&k, &pretty, ""));
    }
    let attrs = pairs(&row["attributes"]);
    if !attrs.is_empty() {
        out.push(rule(w, " attributes "));
        for (k, v) in attrs {
            out.push(line(&k, &v, term::CYAN));
        }
    }
    for (label, key) in [(" events ", "events"), (" links ", "links")] {
        let items = row[key].as_vec().map_or(&[][..], |v| v.as_slice());
        if items.is_empty() {
            continue;
        }
        out.push(rule(w, label));
        for it in items {
            for (k, v) in pairs(it) {
                if k == "attributes" {
                    continue;
                }
                out.push(line(&k, &v, ""));
            }
            for (k, v) in pairs(&it["attributes"]) {
                out.push(line(&format!("  {k}"), &v, term::CYAN));
            }
        }
    }
    out
}

fn help(w: usize) -> Vec<String> {
    const TEXT: &[(&str, &str)] = &[
        ("", ""),
        ("1 2 3 / h l", "logs, traces, metrics"),
        ("↑ ↓ / j k", "move the selection"),
        ("PgUp PgDn g G", "page, top, bottom"),
        ("enter", "open the selection (metrics: load the series)"),
        ("t", "open the trace this row points at"),
        ("tab", "metrics: switch between names and series"),
        ("esc", "back out of a detail, trace or help view"),
        ("/", "edit the filter, enter to apply"),
        ("[ ]", "shrink or grow the time window"),
        ("+ -", "halve or double the row limit"),
        ("r", "re-run the query"),
        ("q", "quit"),
        ("", ""),
        ("filter syntax", "space-separated terms, all AND-ed"),
        (
            "  service.name=checkout",
            "an attribute, matched at all three levels",
        ),
        (
            "  severity_number>=17",
            "a root column: = != < <= > >= and ~ for contains",
        ),
        (
            "  body~\"connection refused\"",
            "quote a value that has spaces in it",
        ),
        (
            "  refused",
            "a word on its own searches body, or a span's name",
        ),
        ("", ""),
        (
            "on a local directory",
            "queries run in-process; no server needs to be up",
        ),
    ];
    TEXT.iter()
        .map(|(k, v)| {
            let mut r = Row::new(w);
            r.put(term::BOLD, &format!("  {k:<28}"));
            r.put(term::DIM, v);
            r.done()
        })
        .collect()
}

// ---- small helpers --------------------------------------------------------

fn body_h(h: usize) -> usize {
    h.saturating_sub(4).max(1)
}

/// First visible index of a list scrolled to keep `sel` on screen.
fn window_start(sel: usize, height: usize, len: usize) -> usize {
    if len <= height {
        return 0;
    }
    sel.saturating_sub(height / 2).min(len - height)
}

fn rule(w: usize, title: &str) -> String {
    let mut r = Row::new(w);
    r.put(term::DIM, "──");
    if !title.is_empty() {
        r.put(term::DIM, title);
    }
    let left = r.left();
    r.repeat(term::DIM, '─', left);
    r.done()
}

fn array(y: &Yaml) -> Vec<Yaml> {
    y.as_vec().cloned().unwrap_or_default()
}

/// A mapping's entries as `(key, rendered value)`, in the order they arrived.
fn pairs(y: &Yaml) -> Vec<(String, String)> {
    y.as_hash()
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), text(v))))
                .collect()
        })
        .unwrap_or_default()
}

/// One value on one line.
///
/// Nested values are rendered inline rather than summarised as `[2 items]`: an
/// array attribute, a kvlist attribute and a structured body are decoded by the
/// engine at some cost, and the detail pane is the one place the reader asked to
/// see them. `Row` clips the line at the pane width, so this only has to be
/// compact, not fitted.
///
/// ponytail: the whole value is built and then clipped, so a thousand-element
/// array costs a string nobody sees, once per frame. Depth is capped because
/// nesting is what makes that unbounded; cap the element count too if a payload
/// that wide ever turns up.
fn text(y: &Yaml) -> String {
    nested(y, 4)
}

fn nested(y: &Yaml, depth: usize) -> String {
    match y {
        Yaml::String(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Real(r) => r.clone(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => "null".into(),
        Yaml::Array(a) if depth == 0 => format!("[{} items]", a.len()),
        Yaml::Hash(h) if depth == 0 => format!("{{{} keys}}", h.len()),
        Yaml::Array(a) => {
            let items: Vec<String> = a.iter().map(|v| nested(v, depth - 1)).collect();
            format!("[{}]", items.join(", "))
        }
        Yaml::Hash(h) => {
            let items: Vec<String> = h
                .iter()
                // A non-string key cannot happen in decoded OTLP, but rendering
                // it beats dropping the pair it belongs to.
                .map(|(k, v)| format!("{}: {}", nested(k, 0), nested(v, depth - 1)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        _ => String::new(),
    }
}

fn num(y: &Yaml) -> Option<f64> {
    match y {
        Yaml::Integer(i) => Some(*i as f64),
        Yaml::Real(r) => r.parse().ok(),
        _ => None,
    }
}

fn service(row: &Yaml) -> &str {
    row["attributes"]["service.name"].as_str().unwrap_or("-")
}

fn sev_style(n: i64) -> &'static str {
    match n {
        17.. => term::RED,
        13..=16 => term::YELLOW,
        9..=12 => term::GREEN,
        _ => term::DIM,
    }
}

fn kind(k: i64) -> &'static str {
    match k {
        1 => "internal",
        2 => "server",
        3 => "client",
        4 => "producer",
        5 => "consumer",
        _ => "unspecified",
    }
}

fn clip(s: &str, n: usize) -> String {
    match n > 0 && s.chars().count() > n {
        true => s.chars().take(n.saturating_sub(1)).collect::<String>() + "…",
        false => s.to_owned(),
    }
}

/// Local wall-clock time of a nanosecond timestamp.
///
/// `localtime_r` rather than a date crate: `libc` is already here, and the only
/// hard part of civil time — the zone — is exactly the part a hand-rolled
/// version would get wrong.
fn hms(ns: i64) -> String {
    let (tm, ms) = civil(ns);
    format!(
        "{:02}:{:02}:{:02}.{ms:03}",
        tm.tm_hour, tm.tm_min, tm.tm_sec
    )
}

fn stamp(ns: i64) -> String {
    let (tm, ms) = civil(ns);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{ms:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn civil(ns: i64) -> (libc::tm, i64) {
    let secs = ns.div_euclid(1_000_000_000) as libc::time_t;
    let ms = ns.rem_euclid(1_000_000_000) / 1_000_000;
    // SAFETY: `tm` is integers plus a `tm_zone` pointer, and null is a valid
    // value for a raw pointer, so all-zero is a valid `tm`. It has to be: the
    // return value is discarded below, and `localtime_r` returns NULL without
    // writing anything for a `time_t` it cannot represent. The zeroed struct
    // then renders as 1900-01-01 — a wrong timestamp, not uninitialised memory.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both arguments are live, correctly typed locals. The `_r` form
    // writes only into `tm` and returns no pointer into shared state, so unlike
    // `localtime` nothing here can be clobbered by another thread's call.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    (tm, ms)
}

fn dur(ns: i64) -> String {
    match ns {
        n if n >= 60_000_000_000 => format!(
            "{}m{:02}s",
            n / 60_000_000_000,
            n % 60_000_000_000 / 1_000_000_000
        ),
        n if n >= 1_000_000_000 => format!("{:.2}s", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.2}ms", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}µs", n as f64 / 1e3),
        n => format!("{n}ns"),
    }
}

fn ms(d: std::time::Duration) -> String {
    match d.as_secs_f64() {
        s if s >= 1.0 => format!("{s:.2}s"),
        s => format!("{:.1}ms", s * 1e3),
    }
}

/// A number at human precision: metric values span counters in the millions and
/// ratios below one, and neither reads well under the other's format.
fn g(v: f64) -> String {
    match v.abs() {
        0.0 => "0".into(),
        x if x >= 1e6 => format!("{:.1}M", v / 1e6),
        x if x >= 1e3 => format!("{:.1}k", v / 1e3),
        x if x >= 1.0 => format!("{v:.1}"),
        _ => format!("{v:.3}"),
    }
}

fn stats_line(s: &Yaml) -> String {
    format!(
        "{}/{} blocks · {} rows scanned · {} matched",
        s["blocks_scanned"].as_i64().unwrap_or(0),
        s["blocks_total"].as_i64().unwrap_or(0),
        s["rows_scanned"].as_i64().unwrap_or(0),
        s["rows_matched"].as_i64().unwrap_or(0),
    )
}

/// Eight levels of block, scaled between the run's own min and max.
///
/// Relative rather than absolute because a flat series at 8 191 and a flat
/// series at 3 are the same shape, and the shape is what a sparkline is for —
/// the numbers are printed beside it.
fn spark(v: &[f64], w: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if v.is_empty() || w == 0 {
        return String::new();
    }
    let (lo, hi) = (
        v.iter().cloned().fold(f64::INFINITY, f64::min),
        v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    // More points than columns: average each column's bucket rather than
    // sampling one of them, so a spike between two samples is not invisible.
    (0..w.min(v.len()))
        .map(|i| {
            let (a, b) = (
                i * v.len() / w.min(v.len()),
                (i + 1) * v.len() / w.min(v.len()),
            );
            let bucket = &v[a..b.max(a + 1).min(v.len())];
            let peak = bucket.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let n = ((peak - lo) / span * 7.0).round().clamp(0.0, 7.0) as usize;
            BARS[n]
        })
        .collect()
}

fn trace_query(id: &str) -> String {
    let mut j = Json::new();
    j.obj(|j| {
        j.key("signal");
        j.str("traces");
        // Deliberately wider than the view's own window. A trace reached from a
        // metric exemplar or an old log line is frequently outside it, and
        // `trace_id = <hex>` is the one filter with a Bloom sidecar behind it —
        // this opens one block out of all of retention, not all of them.
        j.key("from");
        j.str("-3650d");
        j.key("to");
        j.str("now");
        j.key("limit");
        j.i64(2_000);
        j.key("where");
        j.arr(|j| {
            j.obj(|j| {
                j.key("field");
                j.str("trace_id");
                j.key("eq");
                j.str(id);
            });
        });
    });
    j.into_string()
}

const OPS: [(&str, &str); 7] = [
    (">=", "gte"),
    ("<=", "lte"),
    ("!=", "ne"),
    ("=", "eq"),
    ("~", "contains"),
    (">", "gt"),
    ("<", "lt"),
];

/// One piece of a filter line: a `key op value`, or a word with no operator.
///
/// A bare word is kept rather than discarded because only the caller knows what
/// it should mean — see [`Tab::free_text`].
#[derive(Debug, PartialEq)]
enum Part {
    Term(String, &'static str, String),
    Word(String),
}

/// Split the filter line into query terms.
///
/// `service.name=checkout severity_number>=17 body~"connection refused"`. It is
/// deliberately not the API's KYAML grammar: that one is for programs, and no
/// one types `{"attr":"service.name","eq":"checkout"}` into a filter box. Both
/// end up as the same [`Term`](mira_core::query::Term) either way.
fn parse_filter(s: &str) -> Vec<Part> {
    let mut out = Vec::new();
    for tok in tokens(s) {
        // Earliest operator wins, longest at that position — otherwise `>=`
        // parses as `>` with a value of `=17`.
        let mut best: Option<(usize, usize, &'static str)> = None;
        for (sym, op) in OPS {
            if let Some(p) = tok.find(sym) {
                let better = best.is_none_or(|(bp, bl, _)| p < bp || (p == bp && sym.len() > bl));
                if better {
                    best = Some((p, sym.len(), op));
                }
            }
        }
        match best {
            // A half-written term (`k=`, `=v`) is dropped: the operator says
            // what was meant and it is not there yet.
            Some((p, l, op)) => {
                let (k, v) = (tok[..p].trim(), tok[p + l..].trim());
                if !k.is_empty() && !v.is_empty() {
                    out.push(Part::Term(k.to_owned(), op, v.to_owned()));
                }
            }
            None => out.push(Part::Word(tok)),
        }
    }
    out
}

/// Whitespace-separated, except inside double quotes.
fn tokens(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Type a filter value the way its text reads.
///
/// Ids stay strings whatever they look like: a 16-hex-digit span id made
/// entirely of decimal digits parses as an integer, and comparing a
/// `FixedSizeBinary` column against one matches nothing at all — silently,
/// because an inapplicable term is defined to return no rows rather than an
/// error.
fn scalar(j: &mut Json, key: &str, v: &str) {
    if v == "true" || v == "false" {
        return j.bool(v == "true");
    }
    if !key.ends_with("_id") && !key.ends_with(".id") {
        if let Ok(n) = v.parse::<i64>() {
            return j.i64(n);
        }
        if let Ok(f) = v.parse::<f64>() {
            return j.f64(f);
        }
    }
    j.str(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(k: &str, op: &'static str, v: &str) -> Part {
        Part::Term(k.into(), op, v.into())
    }

    #[test]
    fn filter_terms_split_on_the_longest_operator() {
        let f = parse_filter("service.name=checkout severity_number>=17 http.route~/api");
        assert_eq!(
            f,
            vec![
                term("service.name", "eq", "checkout"),
                term("severity_number", "gte", "17"),
                term("http.route", "contains", "/api"),
            ]
        );
        // `!=` must not be read as `=` with a key ending in `!`.
        assert_eq!(parse_filter("k!=v"), vec![term("k", "ne", "v")]);
        // A word with no operator survives parsing; what it means is the tab's
        // business, not this function's.
        assert_eq!(
            parse_filter("justawordse"),
            vec![Part::Word("justawordse".into())]
        );
        // A half-written term is still dropped: the operator says what was
        // meant, and it is not there yet.
        assert!(parse_filter("=v k=").is_empty());
    }

    #[test]
    fn a_quoted_value_keeps_its_spaces() {
        assert_eq!(
            parse_filter("body~\"connection refused\" a=b"),
            vec![
                term("body", "contains", "connection refused"),
                term("a", "eq", "b"),
            ]
        );
    }

    /// The finding this fixes: a word with no operator used to be dropped on
    /// the floor while the filter bar went on displaying it, so the rows looked
    /// filtered and were not.
    #[test]
    fn a_bare_word_searches_the_tabs_message_column() {
        use mira_core::query::{Op, Target, Value};

        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.filter = "\"connection refused\" service.name=checkout".into();
        let q = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        assert_eq!(q.terms.len(), 2);
        assert!(matches!(&q.terms[0].target, Target::Field(k) if k == "body"));
        assert_eq!(q.terms[0].op, Op::Contains);
        assert_eq!(q.terms[0].value, Value::Str("connection refused".into()));

        // On traces the message column is the span name, not the body.
        app.tab = Tab::Traces;
        let q = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        assert!(matches!(&q.terms[0].target, Target::Field(k) if k == "name"));

        // A word that reads as a number stays text: `body` is a string column
        // and an integer there would match nothing, silently.
        app.tab = Tab::Logs;
        app.filter = "500".into();
        let q = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        assert_eq!(q.terms[0].value, Value::Str("500".into()));
    }

    /// Metrics has no message column, so the word is named in the status line
    /// instead of silently doing nothing.
    #[test]
    fn a_bare_word_on_the_metrics_tab_says_it_was_ignored() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.tab = Tab::Metrics;
        app.filter = "refused pod=a".into();
        assert_eq!(app.ignored_word().as_deref(), Some("refused"));

        // The rest of the line still applies — the word is dropped from the
        // query, not the whole filter.
        let q = crate::api::parse_series(&app.series_query(), 0).unwrap();
        assert_eq!(q.terms.len(), 1);

        // And no tab with a message column ever reports one.
        for tab in [Tab::Logs, Tab::Traces] {
            app.tab = tab;
            assert_eq!(app.ignored_word(), None);
        }
    }

    /// The filter box has to produce a document the API's own parser accepts,
    /// and has to route each term to `field` or `attr` correctly — an `attr`
    /// term against a root column matches nothing, silently.
    #[test]
    fn the_filter_compiles_to_a_query_the_api_parses() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.tab = Tab::Traces;
        app.filter =
            "service.name=checkout duration_nano>=500000 trace_id=00112233445566778899aabbccddeeff"
                .into();
        let body = app.rows_query();
        let q = crate::api::parse_search(&body, 1_000_000_000_000_000_000).unwrap();

        use mira_core::query::{Op, Signal, Target, Value};
        assert_eq!(q.signal, Signal::Traces);
        assert_eq!(q.limit, 200);
        assert_eq!(q.from, 1_000_000_000_000_000_000 - 3_600_000_000_000);
        assert_eq!(q.terms.len(), 3);
        // Not a root column of `spans`, so it has to be an attribute.
        assert!(matches!(&q.terms[0].target, Target::Attr(k) if k == "service.name"));
        assert!(matches!(&q.terms[1].target, Target::Field(k) if k == "duration_nano"));
        assert_eq!(q.terms[1].op, Op::Gte);
        assert_eq!(q.terms[1].value, Value::Int(500_000));
        // An id stays a string even though this one is all hex.
        assert!(matches!(&q.terms[2].value, Value::Str(s) if s.len() == 32));
    }

    /// The same key is a field on one signal and an attribute on another, and
    /// the tab is the only thing that knows which.
    #[test]
    fn the_signal_decides_whether_a_key_is_a_column() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.filter = "name=checkout".into();
        let logs = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        app.tab = Tab::Traces;
        let traces = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        assert!(matches!(&logs.terms[0].target, Target::Attr(_)));
        assert!(matches!(&traces.terms[0].target, Target::Field(_)));
        use mira_core::query::Target;
    }

    /// Every root column the engine can compare has to be in `fields`, or the
    /// filter box sends it as `attr` and the Bloom filter prunes it to nothing.
    /// Pinned against the schema so the next column added there cannot repeat
    /// what happened to `event_name`.
    ///
    /// The browser UI keeps a second copy of the same list and, being
    /// JavaScript, cannot read `schema.rs` — so it is pinned here too rather
    /// than against literals of its own. `include_str!` is what makes that
    /// work: the file is a compile-time input, so adding a column to the
    /// schema fails this test until *both* filter boxes know about it.
    #[test]
    fn the_field_list_is_the_schema() {
        // Left out on purpose: `id`, `resource_id` and `scope_id` are
        // block-local numbers that mean nothing to whoever is typing, and
        // `body_ser` is Binary, which `field_pred` has no comparison for.
        let skip = ["id", "resource_id", "scope_id", "body_ser"];
        let js = include_str!("../ui/src/lib/api.js");
        for (tab, key, schema) in [
            (Tab::Logs, "logs", &mira_core::schema::LOGS),
            (Tab::Traces, "traces", &mira_core::schema::SPANS),
        ] {
            let mut want: Vec<&str> = schema
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .filter(|n| !skip.contains(n))
                .collect();
            let mut got = tab.fields().to_vec();
            // Every odd field of a split on `'` is a quoted element, which is
            // enough parsing for a list of bare identifiers.
            let arr = js
                .split_once(&format!("\n  {key}: ["))
                .expect("FIELDS key")
                .1;
            let mut browser: Vec<&str> = arr[..arr.find(']').expect("closing bracket")]
                .split('\'')
                .skip(1)
                .step_by(2)
                .collect();
            want.sort_unstable();
            got.sort_unstable();
            browser.sort_unstable();
            assert_eq!(got, want, "{tab:?}");
            assert_eq!(browser, want, "{tab:?} in ui/src/lib/api.js");
        }
    }

    /// An array or kvlist attribute is decoded by the engine at real cost, and
    /// the detail pane is where the reader asked to see it — `[2 items]` there
    /// throws the answer away.
    #[test]
    fn nested_values_render_inline_down_to_a_depth() {
        let v = |s: &str| text(&crate::api::parse(s).unwrap()["v"]);
        assert_eq!(v(r#"{"v":["mira","serve"]}"#), "[mira, serve]");
        assert_eq!(v(r#"{"v":{"role":"user","n":2}}"#), "{role: user, n: 2}");
        assert_eq!(v(r#"{"v":[{"type":"text"}]}"#), "[{type: text}]");
        // Capped, so a pathological nest cannot spend a frame building a line
        // that gets clipped at the pane width anyway.
        assert_eq!(v(r#"{"v":[[[[["deep"]]]]]}"#), "[[[[[1 items]]]]]");
    }

    #[test]
    fn the_trace_query_is_the_bloom_indexed_shape() {
        let q =
            crate::api::parse_search(&trace_query("abababababababababababababababab"), 0).unwrap();
        use mira_core::query::{Op, Target};
        assert_eq!(q.terms.len(), 1);
        assert!(matches!(&q.terms[0].target, Target::Field(f) if f == "trace_id"));
        assert_eq!(q.terms[0].op, Op::Eq);
    }

    /// A waterfall is read parent-then-children, and the child order is by
    /// start time. Reconstructing that from `parent_span_id` is the only real
    /// logic in the trace view.
    #[test]
    fn spans_order_depth_first_with_orphans_kept() {
        let doc = crate::api::parse(
            r#"{"rows":[
              {"span_id":"02","parent_span_id":"01","start_time_unix_nano":30,"duration_nano":5},
              {"span_id":"01","start_time_unix_nano":10,"duration_nano":100},
              {"span_id":"03","parent_span_id":"01","start_time_unix_nano":20,"duration_nano":5},
              {"span_id":"04","parent_span_id":"03","start_time_unix_nano":21,"duration_nano":1},
              {"span_id":"09","parent_span_id":"ff","start_time_unix_nano":90,"duration_nano":1}
            ]}"#,
        )
        .unwrap();
        let t = Trace::new("abc".into(), &array(&doc["rows"]));
        let seen: Vec<(&str, usize)> = t
            .spans
            .iter()
            .map(|(s, d)| (s["span_id"].as_str().unwrap(), *d))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("01", 0),
                // 03 starts before 02, and 04 hangs off 03.
                ("03", 1),
                ("04", 2),
                ("02", 1),
                // Parent never arrived; it is still part of the trace.
                ("09", 0),
            ]
        );
        assert_eq!(t.t0, 10);
        assert_eq!(t.span_ns, 100);
    }

    /// A cyclic `parent_span_id` is corrupt data, not a reason to hang or to
    /// quietly show fewer spans than the trace has.
    #[test]
    fn a_parent_cycle_still_shows_every_span() {
        let doc = crate::api::parse(
            r#"{"rows":[
              {"span_id":"01","parent_span_id":"02","start_time_unix_nano":10,"duration_nano":1},
              {"span_id":"02","parent_span_id":"01","start_time_unix_nano":20,"duration_nano":1}
            ]}"#,
        )
        .unwrap();
        let t = Trace::new("abc".into(), &array(&doc["rows"]));
        assert_eq!(t.spans.len(), 2);
    }

    #[test]
    fn sparkline_scales_between_the_runs_own_bounds() {
        assert_eq!(
            spark(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 8),
            "▁▂▃▄▅▆▇█"
        );
        // Flat is flat, not a division by zero.
        assert_eq!(spark(&[5.0; 4], 4), "▁▁▁▁");
        assert_eq!(spark(&[], 8), "");
        // More points than columns: each column takes its bucket's peak, so the
        // spike survives.
        assert!(spark(&[0.0, 0.0, 9.0, 0.0], 2).ends_with('█'));
    }

    /// `max_points` truncation keeps the newest points, so a capped sparkline
    /// is the tail of the window drawn under a filter bar that still says
    /// `last 24h`. The engine reports the count; the only failure is not
    /// showing it.
    #[test]
    fn a_truncated_sparkline_says_how_many_points_are_missing() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        let series = |extra: &str| {
            // A full sparkline and a wide range, so the badge is the field
            // that would be clipped if its width were not reserved.
            let pts: Vec<String> = (0..400)
                .map(|i| format!("[{i},{}]", (i - 200) * 1000))
                .collect();
            array(
                &crate::api::parse(&format!(
                    r#"{{"series":[{{"name":"m","attributes":{{}},{extra}"points":[{}]}}]}}"#,
                    pts.join(",")
                ))
                .unwrap()["series"],
            )
        };

        app.series = series(r#""dropped_points":8240,"#);
        let capped = strip(&app.series_lines(80)[1]);
        assert!(capped.contains("+8240 dropped"), "{capped:?}");

        // Reserved out of the bar rather than appended past the edge: `Row`
        // clips at the right margin, and this is the field that must not be
        // the one it clips.
        assert!(capped.trim_end().ends_with("+8240 dropped"), "{capped:?}");

        // And no badge at all when the whole window fitted, rather than a `+0`.
        app.series = series("");
        let whole = strip(&app.series_lines(80)[1]);
        assert!(!whole.contains("dropped"), "{whole:?}");
    }

    /// Every pane clamps its own cursor, and an empty result is the case that
    /// gets it wrong.
    #[test]
    fn cursors_stay_in_range_on_an_empty_result() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.move_by(5);
        app.move_to(usize::MAX);
        assert_eq!(app.sel, 0);
        app.rows = vec![Yaml::Null, Yaml::Null, Yaml::Null];
        app.move_to(usize::MAX);
        assert_eq!(app.sel, 2);
        app.move_by(10);
        assert_eq!(app.sel, 2);
        app.move_by(-10);
        assert_eq!(app.sel, 0);

        // The scrolling panes report `usize::MAX` for their length, which is a
        // negative `isize`. Clamping there used to panic on the first `j`.
        for mode in [Mode::Detail, Mode::Help] {
            app.mode = mode;
            app.scroll = 0;
            app.move_by(3);
            assert_eq!(app.scroll, 3, "{mode:?}");
            app.move_by(-9);
            assert_eq!(app.scroll, 0, "{mode:?}");
        }
    }

    /// Painting must never panic and never overrun, whatever the terminal size
    /// or the mode — a panic here leaves the user's shell in raw mode.
    #[test]
    fn every_view_fits_the_frame_at_any_size() {
        let doc = crate::api::parse(
            r#"{"rows":[{"time_unix_nano":1788877362987743417,"severity_number":17,
                 "severity_text":"ERROR","body":"boom","duration_nano":5000000,
                 "status_code":2,"name":"GET /x","span_id":"01",
                 "trace_id":"abababababababababababababababab",
                 "events":[{"name":"ev","time_unix_nano":1788877362987743500}],
                 "links":[{"trace_id":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}],
                 "attributes":{"service.name":"checkout"}}],
               "series":[{"name":"m","attributes":{"service.name":"c"},
                 "points":[[1,2],[2,3]],
                 "exemplars":[{"trace_id":"abababababababababababababababab"}]}],
               "names":[{"name":"http.server.duration","unit":"ms","kind":"histogram"}]}"#,
        )
        .unwrap();
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.rows = array(&doc["rows"]);
        app.series = array(&doc["series"]);
        app.names = array(&doc["names"]);
        app.trace = Some(Trace::new("ab".into(), &array(&doc["rows"])));
        app.stats = "x".into();

        for (w, h) in [(40, 8), (80, 24), (200, 60), (41, 9)] {
            for tab in [Tab::Logs, Tab::Traces, Tab::Metrics] {
                for mode in [
                    Mode::List,
                    Mode::Filter,
                    Mode::Detail,
                    Mode::Trace,
                    Mode::Span,
                    Mode::Help,
                ] {
                    app.tab = tab;
                    app.mode = mode;
                    let f = app.frame(w, h);
                    assert_eq!(f.len(), h, "row count at {w}x{h}");
                    for line in &f {
                        let visible = line
                            .chars()
                            .scan(false, |esc, c| {
                                Some(match (*esc, c) {
                                    (_, '\x1b') => {
                                        *esc = true;
                                        None
                                    }
                                    (true, c) => {
                                        *esc = !c.is_ascii_alphabetic();
                                        None
                                    }
                                    (false, c) => Some(c),
                                })
                            })
                            .flatten()
                            .count();
                        assert!(visible <= w, "line of {visible} columns at width {w}");
                    }
                }
            }
        }

        // Under the minimum there is no frame to fit. Drawing the 40-column one
        // anyway wraps every row — `Term::draw` has no cursor addressing — and
        // scrolls its own top off the screen for good, so it says so instead,
        // at the width the terminal actually has.
        for (w, h) in [(30, 6), (80, 7), (39, 24), (0, 0)] {
            let f = app.frame(w, h);
            assert_eq!(f.len(), 1, "{w}x{h} is one line, not a frame");
            assert!(strip(&f[0]).chars().count() <= w, "{w}x{h}");
        }
        assert_eq!(
            strip(&app.frame(30, 6)[0]),
            "terminal too small — need 40x8"
        );
    }

    const W: usize = 100;
    const H: usize = 24;

    /// A block directory with all three signals in it, timestamped now, because
    /// every query the TUI issues is relative to the clock.
    fn store(name: &str) -> std::path::PathBuf {
        use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::metrics::v1::metric::Data;
        use mira_proto::metrics::v1::{
            AggregationTemporality, Exemplar, Gauge, Metric, NumberDataPoint, ResourceMetrics,
            ScopeMetrics, Sum, exemplar, number_data_point,
        };
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

        let dir = std::env::temp_dir().join(format!("mira-tui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = crate::api::now_nanos() as u64;
        let node = mira_core::block::node_id("a");

        let mut b = mira_core::logs::LogsBuilder::new();
        b.append_request(&crate::e2e::logs_export(
            "checkout",
            now - 60_000_000_000,
            5,
        ))
        .unwrap();
        mira_core::block::publish(&dir, "logs", node, 0, &b.finish().unwrap()).unwrap();

        // The same trace id `logs_export` stamps on its records, so `t` on a log
        // line has somewhere to go.
        let trace_id = vec![0xabu8; 16];
        let mut b = mira_core::traces::TracesBuilder::new();
        b.append_request(&ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: (0..4u64)
                        .map(|i| Span {
                            trace_id: trace_id.clone().into(),
                            span_id: vec![i as u8 + 1; 8].into(),
                            // A tree, not a list: the waterfall's indenting and
                            // its parent lookup are the point of the view.
                            parent_span_id: match i {
                                0 => Vec::new(),
                                _ => vec![i as u8; 8],
                            }
                            .into(),
                            name: format!("GET /checkout/{i}"),
                            start_time_unix_nano: now - 60_000_000_000 + i * 1_000_000,
                            end_time_unix_nano: now - 59_000_000_000 + i * 1_000_000,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
        .unwrap();
        mira_core::block::publish(&dir, "traces", node, 0, &b.finish().unwrap()).unwrap();

        let point = |i: u64| NumberDataPoint {
            time_unix_nano: now - 60_000_000_000 + i * 1_000_000_000,
            value: Some(number_data_point::Value::AsDouble(i as f64 * 1.5)),
            exemplars: vec![Exemplar {
                time_unix_nano: now - 60_000_000_000,
                trace_id: trace_id.clone().into(),
                value: Some(exemplar::Value::AsDouble(1.0)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut b = mira_core::metrics::MetricsBuilder::new();
        b.append_request(&ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                scope_metrics: vec![ScopeMetrics {
                    metrics: vec![
                        Metric {
                            name: "http.server.duration".into(),
                            unit: "ms".into(),
                            data: Some(Data::Gauge(Gauge {
                                data_points: (0..6).map(point).collect(),
                            })),
                            ..Default::default()
                        },
                        Metric {
                            name: "http.server.requests".into(),
                            unit: "1".into(),
                            data: Some(Data::Sum(Sum {
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                                is_monotonic: true,
                                data_points: (0..6).map(point).collect(),
                            })),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
        .unwrap();
        mira_core::block::publish(&dir, "metrics", node, 0, &b.finish().unwrap()).unwrap();
        dir
    }

    /// One turn of the loop in [`run`], minus the terminal.
    ///
    /// The order matters and is the reason it is a helper rather than a call to
    /// `key`: the frame is painted *before* the deferred job runs, so the screen
    /// that says "running" is on it while the query blocks. Anything asserting on
    /// what the user sees has to go through the same sequence.
    fn settle(app: &mut App) -> String {
        let mut f = app.frame(W, H);
        for _ in 0..8 {
            let Some(job) = app.job.take() else {
                return f.join("\n");
            };
            app.run(job, H);
            f = app.frame(W, H);
        }
        panic!("a job kept queueing another one")
    }

    fn press(app: &mut App, k: Key) -> String {
        assert!(app.key(k, H), "quit on {k:?}");
        settle(app)
    }

    fn typed(app: &mut App, s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            out = press(app, Key::Char(c));
        }
        out
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

    /// The whole app driven by keystrokes against a real block directory, which
    /// is what `mira mira --data-dir` does with no server anywhere. `run` itself
    /// is not here — it is this loop plus a `Term`, and a `Term` needs a pty.
    #[test]
    fn a_session_walks_the_three_tabs_and_lands_on_a_trace() {
        let dir = store("session");
        let mut app = App::new(Source::Local(dir));

        // Opening screen: the logs tab, already loaded, with no key pressed.
        let f = strip(&settle(&mut app));
        assert!(f.contains("checkout handled request"), "{f}");
        assert!(f.contains("ERROR"), "{f}");
        assert_eq!(app.rows.len(), 5);

        // Detail, scrolled, and back. Esc leaves the mode, it does not quit.
        let f = strip(&press(&mut app, Key::Enter));
        assert_eq!(app.mode, Mode::Detail);
        assert!(f.contains("service.name"), "{f}");
        press(&mut app, Key::Char('j'));
        assert_eq!(app.scroll, 1);
        press(&mut app, Key::Esc);
        assert_eq!(app.mode, Mode::List);

        // `l` walks right through the tabs; each arrival reloads.
        let f = strip(&press(&mut app, Key::Char('l')));
        assert_eq!(app.tab, Tab::Traces);
        assert!(f.contains("GET /checkout/"), "{f}");
        assert_eq!(app.rows.len(), 4);

        // Metrics loads names, then loads the first series without being asked —
        // the two-job sequence `settle` exists to drain.
        let f = strip(&press(&mut app, Key::Char('l')));
        assert_eq!(app.tab, Tab::Metrics);
        assert!(f.contains("http.server.duration"), "{f}");
        assert!(!app.series.is_empty(), "the first name loaded its series");

        // Tab moves the arrow keys from the name list to the series list.
        assert!(!app.on_series);
        press(&mut app, Key::Tab);
        assert!(app.on_series);
        press(&mut app, Key::Tab);
        press(&mut app, Key::Char('j'));
        assert_eq!(app.nsel, 1);
        let f = strip(&press(&mut app, Key::Enter));
        assert!(f.contains("http.server.requests"), "{f}");

        // A metric exemplar carries the trace id of the request that produced
        // the measurement, and `t` follows it. Three tabs, one key.
        press(&mut app, Key::Tab);
        let f = strip(&press(&mut app, Key::Char('t')));
        assert_eq!(app.mode, Mode::Trace);
        let t = app.trace.as_ref().unwrap();
        assert_eq!(t.id, "abababababababababababababababab");
        assert_eq!(t.spans.len(), 4);
        assert!(f.contains("GET /checkout/0"), "{f}");
        // Indented: the waterfall is a tree, not a list.
        assert!(f.contains("  GET /checkout/1"), "{f}");

        press(&mut app, Key::Down);
        assert_eq!(app.trace.as_ref().unwrap().sel, 1);
        // `t` inside a trace is a no-op rather than a reload of the same trace.
        press(&mut app, Key::Char('t'));
        assert_eq!(app.mode, Mode::Trace);

        // Enter opens the selected span. The waterfall draws a span's shape;
        // its attributes and its status message exist nowhere else, and the
        // span is not in `self.rows` — this trace came from an exemplar.
        let f = strip(&press(&mut app, Key::Enter));
        assert_eq!(app.mode, Mode::Span);
        assert!(f.contains("GET /checkout/1"), "{f}");
        assert!(f.contains("duration_nano"), "{f}");
        press(&mut app, Key::Char('j'));
        assert_eq!(app.scroll, 1, "the span detail scrolls like any other");
        // Back to the waterfall it was opened from, not to the list.
        press(&mut app, Key::Esc);
        assert_eq!(app.mode, Mode::Trace);
        // `q` leaves the mode. Only `q` on the list quits.
        press(&mut app, Key::Char('q'));
        assert_eq!(app.mode, Mode::List);

        // Help is a toggle, and Esc closes it too.
        let f = strip(&press(&mut app, Key::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        assert!(f.contains("open the trace this row points at"), "{f}");
        press(&mut app, Key::Char('?'));
        assert_eq!(app.mode, Mode::List);

        assert!(!app.key(Key::Char('q'), H), "q on the list quits");
    }

    /// The filter bar: typed, edited, applied, and undone. Applying it is the
    /// only thing here that costs a query, which is why Esc restores the text
    /// rather than re-running with the old one.
    #[test]
    fn the_filter_bar_edits_a_query_and_esc_puts_it_back() {
        let dir = store("filter");
        let mut app = App::new(Source::Local(dir));
        settle(&mut app);

        press(&mut app, Key::Char('/'));
        assert_eq!(app.mode, Mode::Filter);
        typed(&mut app, "severity_text=WARN xx");
        assert_eq!(app.filter, "severity_text=WARN xx");
        press(&mut app, Key::Ctrl('w'));
        assert_eq!(app.filter, "severity_text=WARN ");
        press(&mut app, Key::Backspace);
        press(&mut app, Key::Backspace);
        assert_eq!(app.filter, "severity_text=WAR");
        typed(&mut app, "N");

        let f = strip(&press(&mut app, Key::Enter));
        assert_eq!(app.mode, Mode::List);
        assert!(app.rows.is_empty());
        assert!(f.contains("no rows in this window"), "{f}");

        // Esc restores the text that was there before `/`, so an abandoned edit
        // leaves the view exactly as it was found.
        press(&mut app, Key::Char('/'));
        typed(&mut app, " and more");
        press(&mut app, Key::Esc);
        assert_eq!(app.filter, "severity_text=WARN");
        assert_eq!(app.mode, Mode::List);

        press(&mut app, Key::Char('/'));
        press(&mut app, Key::Ctrl('u'));
        press(&mut app, Key::Enter);
        assert_eq!(app.rows.len(), 5, "an empty filter is every row back");

        // A bare word on the metrics tab has no text column to search, so it is
        // reported rather than quietly dropped.
        press(&mut app, Key::Char('3'));
        press(&mut app, Key::Char('/'));
        typed(&mut app, "checkout");
        let f = strip(&press(&mut app, Key::Enter));
        assert!(f.contains("ignored"), "{f}");
    }

    /// The controls that change the query rather than the view, and the two
    /// things that can go wrong: a selection that points at nothing to follow,
    /// and a store that cannot be read.
    #[test]
    fn the_window_and_limit_keys_requery_and_failures_stay_on_screen() {
        let dir = store("window");
        let mut app = App::new(Source::Local(dir.clone()));
        settle(&mut app);

        assert_eq!(app.win, 2);
        press(&mut app, Key::Char('['));
        assert_eq!(app.win, 1);
        for _ in 0..9 {
            press(&mut app, Key::Char(']'));
        }
        assert_eq!(app.win, WINDOWS.len() - 1, "clamped at the widest window");
        let f = strip(&press(&mut app, Key::Char('[')));
        assert!(f.contains("7d"), "{f}");

        assert_eq!(app.limit, 200);
        press(&mut app, Key::Char('+'));
        assert_eq!(app.limit, 400);
        press(&mut app, Key::Char('-'));
        press(&mut app, Key::Char('-'));
        assert_eq!(app.limit, 100);
        press(&mut app, Key::Char('r'));
        assert_eq!(app.rows.len(), 5);

        // Paging and the jump keys share one clamp across every pane.
        press(&mut app, Key::End);
        assert_eq!(app.sel, 4);
        press(&mut app, Key::PageUp);
        assert_eq!(app.sel, 0);
        press(&mut app, Key::PageDown);
        assert_eq!(app.sel, 4);
        press(&mut app, Key::Home);
        assert_eq!(app.sel, 0);

        // A span row has a trace id; a row that does not exist has nothing.
        app.rows.clear();
        let f = strip(&press(&mut app, Key::Char('t')));
        assert!(app.err);
        assert!(f.contains("nothing here carries a trace id"), "{f}");

        // The store going away under the session is a message on the status
        // bar, not a panic and not a blank screen.
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();
        let f = strip(&press(&mut app, Key::Char('r')));
        assert!(app.err, "{f}");
        assert!(
            f.contains("logs"),
            "the failure names what it could not read: {f}"
        );
    }
}
