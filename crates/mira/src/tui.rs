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
        // A terminal narrower than this cannot show a timestamp and a body, and
        // every column computation below would start clamping to zero.
        let (w, h) = (w.max(40), h.max(8));
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

    /// Columns of the signal's root table.
    ///
    /// This is what decides whether `name=checkout` filters a column or an
    /// attribute, and getting it wrong is silent: an unknown `field` matches
    /// nothing rather than erroring, by design, so a typo here would look like
    /// "no results" forever.
    fn fields(self) -> &'static [&'static str] {
        match self {
            Tab::Traces => &[
                "trace_id",
                "span_id",
                "parent_span_id",
                "flags",
                "name",
                "kind",
                "start_time_unix_nano",
                "duration_nano",
                "status_code",
                "status_message",
            ],
            _ => &[
                "time_unix_nano",
                "observed_time_unix_nano",
                "severity_number",
                "severity_text",
                "body",
                "trace_id",
                "span_id",
                "flags",
            ],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Filter,
    Detail,
    Trace,
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
                self.trace = Some(Trace::new(id, spans));
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
            j.key("max_series");
            j.i64(64);
            // A sparkline is one character per point. Asking for the API's
            // default 5 000 would download three orders of magnitude more than
            // the widest terminal can render.
            j.key("max_points");
            j.i64(400);
            j.key("where");
            self.terms(j);
        });
        j.into_string()
    }

    fn terms(&self, j: &mut Json) {
        j.arr(|j| {
            for (key, op, val) in parse_filter(&self.filter) {
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
            Key::Char('q') | Key::Esc => self.mode = Mode::List,
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
                self.on_series = !self.on_series
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
    fn move_by(&mut self, d: isize) {
        let (sel, len) = self.cursor();
        let next = (sel as isize + d).clamp(0, len.saturating_sub(1) as isize) as usize;
        self.set_cursor(next);
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
            (Mode::Detail, _) | (Mode::Help, _) => (self.scroll, usize::MAX),
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
            (Mode::Detail, _) | (Mode::Help, _) => self.scroll = n,
            (Mode::Trace, _) => {
                if let Some(t) = self.trace.as_mut() {
                    t.sel = n
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
            (Mode::Trace, _) => return,
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
        match self.tab {
            Tab::Metrics => self.series.get(self.ssel),
            _ => self.rows.get(self.sel),
        }
    }

    // ---- rendering --------------------------------------------------------

    fn frame(&mut self, w: usize, h: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(h);
        out.push(self.tabbar(w));
        out.push(self.filterbar(w));

        let bh = body_h(h);
        let mut body = match self.mode {
            Mode::Help => help(w),
            Mode::Detail => self.detail_full(w),
            Mode::Trace => self.waterfall(w, bh),
            _ => match self.tab {
                Tab::Metrics => self.metrics(w, bh),
                _ => self.records(w, bh),
            },
        };
        // Scrolling panes hand back every line they have and are windowed here,
        // so each one does not have to reimplement the clamp.
        if matches!(self.mode, Mode::Detail | Mode::Help) {
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
            Mode::Trace => "esc back  ↑↓ span",
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
                    let bars = w.saturating_sub(28).max(8);
                    r.put(term::CYAN, &spark(&pts, bars));
                    r.plain("  ");
                    r.put(term::DIM, &format!("{} → {}", g(lo), g(hi)));
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
    fn new(id: String, spans: Vec<Yaml>) -> Trace {
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

fn text(y: &Yaml) -> String {
    match y {
        Yaml::String(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Real(r) => r.clone(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => "null".into(),
        Yaml::Array(a) => format!("[{} items]", a.len()),
        Yaml::Hash(h) => format!("{{{} keys}}", h.len()),
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
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
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
            })
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

/// Split the filter line into query terms.
///
/// `service.name=checkout severity_number>=17 body~"connection refused"`. It is
/// deliberately not the API's KYAML grammar: that one is for programs, and no
/// one types `{"attr":"service.name","eq":"checkout"}` into a filter box. Both
/// end up as the same [`Term`](mira_core::query::Term) either way.
fn parse_filter(s: &str) -> Vec<(String, &'static str, String)> {
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
        if let Some((p, l, op)) = best {
            let (k, v) = (tok[..p].trim(), tok[p + l..].trim());
            if !k.is_empty() && !v.is_empty() {
                out.push((k.to_owned(), op, v.to_owned()));
            }
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
    j.str(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_terms_split_on_the_longest_operator() {
        let f = parse_filter("service.name=checkout severity_number>=17 http.route~/api");
        assert_eq!(
            f,
            vec![
                ("service.name".into(), "eq", "checkout".into()),
                ("severity_number".into(), "gte", "17".into()),
                ("http.route".into(), "contains", "/api".into()),
            ]
        );
        // `!=` must not be read as `=` with a key ending in `!`.
        assert_eq!(parse_filter("k!=v"), vec![("k".into(), "ne", "v".into())]);
        // A term with no operator, or an empty side, is dropped rather than
        // sent as something the API will reject.
        assert!(parse_filter("justawordse").is_empty());
        assert!(parse_filter("=v k=").is_empty());
    }

    #[test]
    fn a_quoted_value_keeps_its_spaces() {
        assert_eq!(
            parse_filter("body~\"connection refused\" a=b"),
            vec![
                ("body".into(), "contains", "connection refused".into()),
                ("a".into(), "eq", "b".into()),
            ]
        );
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
        let t = Trace::new("abc".into(), array(&doc["rows"]));
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
        let t = Trace::new("abc".into(), array(&doc["rows"]));
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
        app.trace = Some(Trace::new("ab".into(), array(&doc["rows"])));
        app.stats = "x".into();

        for (w, h) in [(40, 8), (80, 24), (200, 60), (41, 9)] {
            for tab in [Tab::Logs, Tab::Traces, Tab::Metrics] {
                for mode in [
                    Mode::List,
                    Mode::Filter,
                    Mode::Detail,
                    Mode::Trace,
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
    }
}
