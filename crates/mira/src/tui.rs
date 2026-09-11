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
        // Follow mode is the read timing out rather than a thread or a channel:
        // the whole UI is one loop, and a key that does not arrive within the
        // interval is exactly the signal to re-run the query.
        match term
            .key(if app.tail { TAIL_MS } else { -1 })
            .map_err(|e| e.to_string())?
        {
            Some(k) if !app.key(k, h) => return Ok(()),
            // A tick, not a reload: `reload` writes "running" over the status
            // bar, and a bar that strobes every three seconds is worse than no
            // bar at all.
            None if app.tail => app.job = Some(Job::Rows),
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
    /// The frame around whatever the filter matched (section 7.3), from `/correlate`.
    Frame,
    /// The service map, from `/map`.
    Map,
    /// The rules this node evaluates and what they are doing, from `/alerts`.
    Alerts,
    /// What the node counts about itself, from `/stats`.
    Diag,
    Help,
}

impl Mode {
    /// Whether this pane scrolls a rendered body rather than selecting a row.
    fn scrolls(self) -> bool {
        matches!(self, Mode::Detail | Mode::Span | Mode::Help | Mode::Diag)
    }
}

enum Job {
    Rows,
    Names,
    Series,
    Trace(String),
    Frame,
    Map,
    Alerts,
    Diag,
}

/// What Enter does to the selected line of the frame or map pane.
enum Pick {
    Service(String),
    Trace(String),
}

/// How the map names the synthetic caller of every root span
/// (`mira_core::frame::ENTRY`). A string where every other node key is a decimal
/// entity key, so it cannot collide with one.
const ENTRY_KEY: &str = "entry";

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
    /// The frame and map panes, and the one cursor they share — only one of the
    /// two is ever on screen, and both are lists of things Enter acts on.
    frame: Option<FrameView>,
    map: Option<MapView>,
    /// The alert list shares the same cursor: it is the third pane that is a
    /// list of things Enter acts on, and only one of the three is ever drawn.
    alerts: Vec<Yaml>,
    /// The whole `/api/v1/stats` document, rendered rather than destructured —
    /// a field added to the endpoint should appear here without a code change.
    diag: Option<Yaml>,
    psel: usize,
    /// Re-run the list query on a timer instead of blocking on a key.
    tail: bool,
    scroll: usize,
    stats: String,
    status: String,
    err: bool,
    job: Option<Job>,
}

/// How long a follow tick waits for a key before giving up and re-querying.
///
/// The same three seconds the browser UI polls on, and for the same reason: a
/// read here is single-digit milliseconds, so the interval is chosen for how
/// often a person wants the screen to change, not for what the engine can take.
const TAIL_MS: i32 = 3000;

/// What `/correlate` came back with, flattened into the two lists it draws.
///
/// Services are grouped by name because an entity is an *instance* (section 7.2):
/// three replicas of one service are three entities, which matters to the
/// engine and not to someone reading a strip of names.
struct FrameView {
    from: i64,
    to: i64,
    truncated: bool,
    services: Vec<(String, usize)>,
    traces: Vec<String>,
}

/// The service map as a call tree rather than a graph.
///
/// A terminal draws a tree well and a graph badly, and the tree is the reading
/// that answers the question anyway: what calls what, and where does it start.
/// One line per node, so every line is something Enter can act on — the same
/// shape as the waterfall, which is the other tree in this UI.
struct MapView {
    rows: Vec<(usize, Yaml)>,
    unresolved: i64,
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
            frame: None,
            map: None,
            alerts: Vec::new(),
            diag: None,
            psel: 0,
            tail: false,
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
            Job::Frame => self.src.post(source::CORRELATE, &self.frame_query()),
            Job::Map => self.src.post(source::MAP, &self.window_query()),
            Job::Alerts => self.src.get(source::ALERTS),
            Job::Diag => self.src.get(source::STATS),
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
        self.stats = match job {
            // Neither answers about blocks, so the scan counters would all read
            // zero and look like a query that found nothing.
            Job::Alerts | Job::Diag => ms(t.elapsed()),
            _ => format!("{} · {}", stats_line(&doc["stats"]), ms(t.elapsed())),
        };
        self.status.clear();

        if let Some(w) = self.ignored_word() {
            self.status = format!("ignored {w:?}: metrics filters are attr=value terms");
        }

        match job {
            Job::Rows => {
                self.rows = array(&doc["rows"]);
                // A follow tick must not yank the cursor back to the top:
                // someone watching a stream is usually reading one row while
                // the rest of them move under it.
                match self.tail {
                    true => self.sel = self.sel.min(self.rows.len().saturating_sub(1)),
                    false => {
                        self.sel = 0;
                        self.scroll = 0;
                    }
                }
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
            Job::Frame => {
                let f = FrameView::new(&doc["frame"]);
                if f.services.is_empty() && f.traces.is_empty() {
                    self.status = "nothing matched, so there is no frame to widen".into();
                    return;
                }
                self.frame = Some(f);
                self.mode = Mode::Frame;
                self.psel = 0;
            }
            Job::Alerts => {
                self.alerts = array(&doc["alerts"]);
                self.mode = Mode::Alerts;
                self.psel = 0;
                if self.alerts.is_empty() {
                    // Not "everything is healthy". A node with no rules file
                    // pages nobody, and that is the one thing this pane must not
                    // let a reader assume.
                    self.status =
                        "this node evaluates no rules — set alerts.rules in mira.yaml".into();
                }
            }
            Job::Diag => {
                self.diag = Some(doc);
                self.mode = Mode::Diag;
                self.scroll = 0;
            }
            Job::Map => {
                let m = MapView::new(&doc["map"]);
                if m.rows.is_empty() {
                    // Not an error: a store with logs and no traces is a
                    // perfectly ordinary store, and the map is built from
                    // `parent_span_id` at read time.
                    self.status = "no spans in this window, so there is no map".into();
                    return;
                }
                self.map = Some(m);
                self.mode = Mode::Map;
                self.psel = 0;
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

    /// The same filter the list is showing, expanded into a frame.
    ///
    /// `traces` then `peers`, in that order and not the other: `traces` measures
    /// the real extent of what was found, and `peers` then reads the services
    /// inside it. Reversed, `peers` runs against the window the rows happened to
    /// land in and finds only what is already on screen.
    ///
    /// No `limit`: a frame is bounded by the engine's own entity and trace caps
    /// and says so in `truncated`, which is a different question from how many
    /// rows this pane can draw.
    fn frame_query(&self) -> String {
        let mut j = Json::new();
        j.obj(|j| {
            j.key("signal");
            j.str(self.tab.signal());
            j.key("from");
            j.str(&format!("-{}", WINDOWS[self.win]));
            j.key("to");
            j.str("now");
            j.key("expand");
            j.arr(|j| {
                j.str("traces");
                j.str("peers");
            });
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
                Mode::Frame | Mode::Map => self.follow_pick(),
                Mode::Alerts => self.follow_alert(),
                _ => {}
            },
            Key::Char('t') => self.open_trace(),
            Key::Char('c') if self.mode == Mode::List => match self.tab {
                Tab::Metrics => {
                    self.status = "correlate anchors on logs or traces; switch tab first".into();
                    self.err = true;
                }
                _ => {
                    self.status = "running".into();
                    self.job = Some(Job::Frame);
                }
            },
            Key::Char('m') if self.mode == Mode::List => {
                self.status = "running".into();
                self.job = Some(Job::Map);
            }
            // Both answer for the node rather than for the data, so neither
            // depends on the tab, the filter or the window — and both are
            // reachable from wherever the reader already is.
            Key::Char('a') => {
                self.status = "running".into();
                self.job = Some(Job::Alerts);
            }
            Key::Char('d') => {
                self.status = "running".into();
                self.job = Some(Job::Diag);
            }
            // Follow, in the `tail -f` sense. Only the record list: the metrics
            // tab reloads its name list and reselects, and a pane that
            // reselects under the reader every three seconds is unusable.
            Key::Char('f') => match (self.mode, self.tab) {
                (Mode::List, Tab::Logs | Tab::Traces) => {
                    self.tail = !self.tail;
                    if self.tail {
                        self.reload();
                    }
                }
                _ => {
                    self.status = "follow needs the logs or traces list".into();
                    self.err = true;
                }
            },
            _ => {}
        }
        true
    }

    /// Enter, in the frame or map pane.
    ///
    /// Everything in either pane is a link back into an ordinary query, which is
    /// what the algebra's closure buys (section 7.3): there is nothing selectable here
    /// that lands the reader somewhere they cannot then filter.
    fn follow_pick(&mut self) {
        match self.pick() {
            Some(Pick::Trace(id)) => {
                self.status = format!("loading trace {id}");
                self.job = Some(Job::Trace(id));
            }
            // Anded onto the filter rather than replacing it: the frame is a
            // narrowing step, and whatever is already typed is the reason this
            // frame exists.
            Some(Pick::Service(name)) => {
                let t = service_term(&name);
                // Selecting the same service twice is a double-press, not a
                // request for the term twice.
                if !self.filter.contains(&t) {
                    if !self.filter.is_empty() {
                        self.filter.push(' ');
                    }
                    self.filter.push_str(&t);
                }
                self.mode = Mode::List;
                self.reload();
            }
            None => {}
        }
    }

    /// Enter, on an alert: show the records the rule counted.
    ///
    /// The rule's `where` terms come back from the API already spelled in the
    /// filter-bar grammar (`alert::filter_of`), so this is an assignment rather
    /// than a translation — the alert and the query it fired on cannot drift
    /// into two different filters, because there is only one spelling of them.
    /// The window is left alone: `over` is how the rule counts, and a reader who
    /// has just been paged usually wants more history than that, not less.
    fn follow_alert(&mut self) {
        let Some(a) = self.alerts.get(self.psel) else {
            return;
        };
        self.tab = match a["signal"].as_str() {
            Some("traces") => Tab::Traces,
            _ => Tab::Logs,
        };
        self.filter = a["filter"].as_str().unwrap_or_default().to_owned();
        self.mode = Mode::List;
        self.reload();
    }

    /// What the frame or map cursor is pointing at.
    fn pick(&self) -> Option<Pick> {
        match self.mode {
            Mode::Frame => {
                let f = self.frame.as_ref()?;
                match f.services.get(self.psel) {
                    Some((n, _)) => Some(Pick::Service(n.clone())),
                    None => f
                        .traces
                        .get(self.psel - f.services.len())
                        .map(|t| Pick::Trace(t.clone())),
                }
            }
            Mode::Map => {
                let (_, n) = self.map.as_ref()?.rows.get(self.psel)?;
                // `entry` is synthetic — the caller of every root span — so
                // there is no service behind it to filter on.
                match n["key"].as_str() {
                    Some(ENTRY_KEY) => None,
                    _ => Some(Pick::Service(n["name"].as_str()?.to_owned())),
                }
            }
            _ => None,
        }
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
            (m, _) if m.scrolls() => (self.scroll, usize::MAX),
            (Mode::Trace, _) => (
                self.trace.as_ref().map_or(0, |t| t.sel),
                self.trace.as_ref().map_or(0, |t| t.spans.len()),
            ),
            (Mode::Frame, _) => (self.psel, self.frame.as_ref().map_or(0, FrameView::len)),
            (Mode::Map, _) => (self.psel, self.map.as_ref().map_or(0, |m| m.rows.len())),
            (Mode::Alerts, _) => (self.psel, self.alerts.len()),
            (_, Tab::Metrics) if self.on_series => (self.ssel, self.series.len()),
            (_, Tab::Metrics) => (self.nsel, self.names.len()),
            _ => (self.sel, self.rows.len()),
        }
    }

    fn set_cursor(&mut self, n: usize) {
        match (self.mode, self.tab) {
            (m, _) if m.scrolls() => self.scroll = n,
            (Mode::Trace, _) => {
                if let Some(t) = self.trace.as_mut() {
                    t.sel = n;
                }
            }
            (Mode::Frame | Mode::Map | Mode::Alerts, _) => self.psel = n,
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
            // These panes have their own selection, and the record list's is
            // behind them — following that one would open a trace nobody is
            // pointing at.
            (Mode::Frame | Mode::Map, _) => match self.pick() {
                Some(Pick::Trace(id)) => Some(id),
                _ => None,
            },
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
            Mode::Frame => self.frame_pane(w, bh),
            Mode::Map => self.map_pane(w, bh),
            Mode::Alerts => self.alerts_pane(w, bh),
            Mode::Diag => self.diag_pane(w),
            _ => match self.tab {
                Tab::Metrics => self.metrics(w, bh),
                _ => self.records(w, bh),
            },
        };
        // Scrolling panes hand back every line they have and are windowed here,
        // so each one does not have to reimplement the clamp.
        if self.mode.scrolls() {
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
        let right = format!(
            "{}last {}  limit {}",
            if self.tail { "● follow  " } else { "" },
            WINDOWS[self.win],
            self.limit
        );
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
            Mode::Frame => "esc back  ↑↓ move  enter service→filter, trace→waterfall",
            Mode::Map => "esc back  ↑↓ move  enter filter on this service",
            Mode::Alerts => "esc back  ↑↓ move  enter show the records that fired  a reload",
            Mode::Diag => "esc back  ↑↓ scroll  d reload",
            Mode::Help => "esc back",
            Mode::List if self.tab == Tab::Metrics => {
                "↑↓ move  tab pane  enter load  t trace  m map  a alerts  d node  / filter  [] window  ? help"
            }
            Mode::List => {
                "↑↓ move  enter detail  t trace  c frame  m map  a alerts  d node  f follow  / filter  ? help"
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
                .map(|r| i64_of(&r["duration_nano"]))
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

    /// The frame: the window it measured, the services in it, the traces in it.
    fn frame_pane(&self, w: usize, h: usize) -> Vec<String> {
        let Some(f) = &self.frame else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(h);
        let mut head = Row::new(w);
        head.put(term::BOLD, " frame ");
        head.put(
            term::DIM,
            &format!(
                "{} → {}  ·  {}",
                stamp(f.from),
                stamp(f.to),
                dur((f.to - f.from).max(0))
            ),
        );
        out.push(head.done());
        if f.truncated {
            // A capped frame is a sample, and a sample read as a census is the
            // one way this pane can be confidently wrong.
            let mut r = Row::new(w);
            r.put(
                term::YELLOW,
                " sample — narrow the filter before concluding",
            );
            out.push(r.done());
        }

        out.push(rule(w, &format!(" {} services ", f.services.len())));
        for (i, (name, n)) in f.services.iter().enumerate() {
            let sel = i == self.psel;
            let mut r = Row::new(w);
            r.put(if sel { term::REV } else { "" }, &format!("  {name}"));
            if *n > 1 {
                // Three replicas of one service are three entities and one
                // name. The count is the only thing on screen that says so.
                r.put(term::DIM, &format!("  ×{n}"));
            }
            out.push(r.fill(if sel { term::REV } else { "" }));
        }

        out.push(rule(w, &format!(" {} traces ", f.traces.len())));
        let head = h.saturating_sub(out.len());
        let top = window_start(
            self.psel.saturating_sub(f.services.len()),
            head,
            f.traces.len(),
        );
        for (i, id) in f.traces.iter().enumerate().skip(top).take(head) {
            let sel = f.services.len() + i == self.psel;
            let style = if sel { term::REV } else { "" };
            let mut r = Row::new(w);
            r.put(style, &format!("  {id}"));
            out.push(r.fill(style));
        }
        out
    }

    /// The service map, as a call tree rooted at `entry`.
    fn map_pane(&self, w: usize, h: usize) -> Vec<String> {
        let Some(m) = &self.map else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(h);
        let mut head = Row::new(w);
        head.put(term::BOLD, " map ");
        head.put(term::DIM, &format!("{} services", m.rows.len() - 1));
        if m.unresolved > 0 {
            // Every edge on screen is a lower bound without this: an unresolved
            // span is one whose parent was not in the sample, so its call was
            // never counted against anything.
            head.put(
                term::YELLOW,
                &format!(
                    "  ·  {} spans with a parent outside the sample",
                    m.unresolved
                ),
            );
        }
        out.push(head.done());
        out.push(rule(w, ""));

        let vis = h.saturating_sub(2);
        let top = window_start(self.psel, vis, m.rows.len());
        for (i, (depth, n)) in m.rows.iter().enumerate().skip(top).take(vis) {
            out.push(map_row(n, *depth, i == self.psel, w));
        }
        out
    }

    /// Every rule this node evaluates, firing or not.
    ///
    /// Quiet rules are listed as prominently as loud ones. An alerting screen
    /// that shows only what is on fire cannot answer the question an operator
    /// actually has at 3am — "is the rule I wrote for this even running?" — and
    /// an empty screen then means both "all clear" and "nothing is watching".
    fn alerts_pane(&self, w: usize, h: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(h);
        let firing = self
            .alerts
            .iter()
            .filter(|a| a["state"].as_str() == Some("firing"))
            .count();
        let mut head = Row::new(w);
        head.put(term::BOLD, " alerts ");
        head.put(term::DIM, &format!("{} rules  ·  ", self.alerts.len()));
        head.put(
            if firing > 0 { term::RED } else { term::DIM },
            &format!("{firing} firing"),
        );
        out.push(head.done());
        out.push(rule(w, ""));

        // Two lines each, so the selected rule's predicate is on screen without
        // a keystroke: the filter *is* the explanation of the number.
        let vis = h.saturating_sub(2) / 2;
        let top = window_start(self.psel, vis.max(1), self.alerts.len());
        for (i, a) in self.alerts.iter().enumerate().skip(top).take(vis) {
            let sel = i == self.psel;
            let style = if sel { term::REV } else { "" };
            let state = a["state"].as_str().unwrap_or("?");
            let mut r = Row::new(w);
            r.put(
                style,
                &format!(
                    "  {:<7}",
                    match state {
                        "firing" => "●",
                        "pending" => "◐",
                        _ => "○",
                    }
                ),
            );
            r.put(
                if sel {
                    style
                } else {
                    match state {
                        "firing" => term::RED,
                        "pending" => term::YELLOW,
                        _ => term::GREEN,
                    }
                },
                &format!("{state:<8}"),
            );
            r.put(
                style,
                &format!("{:<28}", clip(a["name"].as_str().unwrap_or("?"), 27)),
            );
            r.put(style, &alert_value(a));
            out.push(r.fill(style));

            let mut r = Row::new(w);
            match a["error"].as_str() {
                // An unevaluated rule is not a quiet one, and the pane says
                // which it is: the state above reads "ok" either way.
                Some(e) => r.put(
                    term::RED,
                    &format!("          {}", clip(e, w.saturating_sub(11))),
                ),
                None => r.put(
                    term::DIM,
                    &format!(
                        "          {}  ·  over {}",
                        clip(
                            match a["filter"].as_str().unwrap_or_default() {
                                "" => "(no filter — every record)",
                                f => f,
                            },
                            w.saturating_sub(30)
                        ),
                        dur(i64_of(&a["over_nano"]))
                    ),
                ),
            };
            out.push(r.done());
        }
        out
    }

    /// What this node counts about itself: `/api/v1/stats`, laid out.
    ///
    /// Every number here is one the ingest path already keeps or the filesystem
    /// already knows, so opening this pane costs a `readdir` and nothing else —
    /// a diagnostics screen that perturbs what it measures is worse than none.
    /// `mmap` residency is deliberately absent: `mincore` over a week of blocks
    /// is thousands of syscalls per refresh, and the page cache is the OS's to
    /// report. ponytail: add it per-block behind an explicit key if a hit rate
    /// ever turns out to be the question someone is actually asking.
    fn diag_pane(&self, w: usize) -> Vec<String> {
        let Some(d) = &self.diag else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut head = Row::new(w);
        head.put(term::BOLD, " node ");
        head.put(
            term::DIM,
            &format!(
                "up {}  ·  peak rss {}  ·  ",
                since(i64_of(&d["uptime_s"])),
                bytes(i64_of(&d["peak_rss_bytes"]) as f64)
            ),
        );
        // The one number on this screen that is a reason to wake someone up.
        let free = num(&d["free_fraction"]);
        head.put(
            match free {
                Some(f) if f < 0.1 => term::RED,
                Some(f) if f < 0.2 => term::YELLOW,
                _ => term::DIM,
            },
            &match free {
                Some(f) => format!("disk {:.0}% free", f * 100.0),
                None => "disk unreadable".into(),
            },
        );
        // Only when it has happened. On every volume Mira is meant to run on
        // this is zero, and a permanent "degraded syncs 0" would train the eye
        // to skip the line on the one node where it is not.
        if i64_of(&d["degraded_syncs"]) > 0 {
            head.put(
                term::YELLOW,
                &format!(
                    "  ·  degraded syncs {}",
                    tally(i64_of(&d["degraded_syncs"]) as f64)
                ),
            );
        }
        out.push(head.done());

        let q = &d["queries"];
        out.push(rule(w, " queries "));
        out.push(kv(w, "served", &tally(num(&q["count"]).unwrap_or(0.0))));
        out.push(kv(
            w,
            "mean",
            &format!("{:.2} ms", num(&q["mean_ms"]).unwrap_or(0.0)),
        ));
        out.push(kv(
            w,
            "max",
            &format!("{:.2} ms", num(&q["max_ms"]).unwrap_or(0.0)),
        ));

        for signal in ["logs", "traces", "metrics"] {
            let s = &d["signals"][signal];
            if s.is_badvalue() {
                continue;
            }
            out.push(rule(w, &format!(" {signal} ")));
            let (rows, on_disk) = (i64_of(&s["rows"]) as f64, i64_of(&s["bytes"]) as f64);
            out.push(kv(w, "rows written", &tally(rows)));
            out.push(kv(
                w,
                "blocks",
                &format!(
                    "{} on disk  ·  {} published",
                    match num(&s["blocks_on_disk"]) {
                        Some(b) => tally(b),
                        // Absent is "the filesystem would not answer", which is
                        // not zero blocks — see the endpoint's own comment.
                        None => "?".into(),
                    },
                    tally(i64_of(&s["blocks_published"]) as f64)
                ),
            ));
            // Bytes per row on disk is the cost-per-GB axis (section 11) measured on
            // this node's own data rather than on a benchmark corpus: Arrow
            // encoding, dictionary sharing and zstd, all of it, in one number.
            out.push(kv(
                w,
                "bytes on disk",
                &match rows > 0.0 {
                    true => format!("{}  ·  {:.0} B/row", bytes(on_disk), on_disk / rows),
                    false => bytes(on_disk),
                },
            ));
            if let Some(age) = num(&s["open_block_age_s"]) {
                out.push(kv(w, "open block", &since(age as i64)));
            }
            let (shed, failed, refused) = (
                i64_of(&s["shed"]),
                i64_of(&s["failed"]),
                i64_of(&s["refused"]),
            );
            let mut r = Row::new(w);
            r.put(term::DIM, &format!("  {:<16}", "rejected"));
            r.put(
                if shed + failed + refused > 0 {
                    term::YELLOW
                } else {
                    ""
                },
                &format!("{shed} shed  ·  {failed} failed  ·  {refused} refused"),
            );
            out.push(r.done());
            // Stalled is the readiness condition, so it is the one line here
            // that is never printed as a zero and never left off when set.
            if let Some(secs) = num(&s["stalled_s"]) {
                let mut r = Row::new(w);
                r.put(term::RED, &format!("  {:<16}", "stalled"));
                r.put(
                    term::RED,
                    &format!(
                        "{} — this node cannot store this signal",
                        since(secs as i64)
                    ),
                );
                out.push(r.done());
            }
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
                    let at = i64_of(&e["time_unix_nano"]) - t.t0;
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

impl FrameView {
    fn new(y: &Yaml) -> FrameView {
        let mut services: Vec<(String, usize)> = Vec::new();
        for e in y["entities"].as_vec().into_iter().flatten() {
            let name = e["name"].as_str().unwrap_or("unknown").to_owned();
            match services.iter_mut().find(|(n, _)| *n == name) {
                Some((_, n)) => *n += 1,
                None => services.push((name, 1)),
            }
        }
        // The engine returns entities sorted by key, which is a hash — so a
        // name-ordered list is the only one that reads the same twice.
        services.sort_by(|a, b| a.0.cmp(&b.0));
        FrameView {
            // `from`/`to` are int64 and so arrive as strings (section 7.6).
            from: i64_of(&y["from"]),
            to: i64_of(&y["to"]),
            truncated: y["truncated"].as_bool().unwrap_or(false),
            services,
            traces: y["traces"]
                .as_vec()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect(),
        }
    }

    fn len(&self) -> usize {
        self.services.len() + self.traces.len()
    }
}

impl MapView {
    /// Depth-first from `entry`, children by descending call volume.
    ///
    /// Volume order rather than name order because the first thing anyone wants
    /// out of a service map is the hot path, and it should be the first thing
    /// they read. A service reachable by two paths is drawn under the first one
    /// reached and not again: a tree with the same subtree in it twice is a tree
    /// nobody can count spans off.
    fn new(y: &Yaml) -> MapView {
        let nodes = y["nodes"].as_vec().map_or(&[][..], Vec::as_slice);
        let edges = y["edges"].as_vec().map_or(&[][..], Vec::as_slice);
        // `entry` is synthetic and so is not in `nodes`, but the tree has to
        // start somewhere and a map that hides where traffic arrives cannot be
        // read. No `spans` or `errors`: it is a caller, not a service.
        let entry = {
            let mut h = yaml_rust2::yaml::Hash::new();
            for k in ["key", "name"] {
                h.insert(Yaml::String(k.into()), Yaml::String(ENTRY_KEY.into()));
            }
            Yaml::Hash(h)
        };

        let mut rows: Vec<(usize, Yaml)> = Vec::with_capacity(nodes.len() + 1);
        if nodes.is_empty() {
            return MapView {
                rows,
                unresolved: 0,
            };
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stack = vec![(ENTRY_KEY.to_owned(), 0usize)];
        while let Some((key, depth)) = stack.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            let node = match key == ENTRY_KEY {
                true => &entry,
                false => match nodes.iter().find(|n| n["key"].as_str() == Some(&key)) {
                    Some(n) => n,
                    // An edge naming a node the sample never reached. Skipping
                    // it loses nothing: its own spans are not in `nodes` either.
                    None => continue,
                },
            };
            rows.push((depth, node.clone()));
            let mut kids: Vec<(&Yaml, u64)> = edges
                .iter()
                .filter(|e| e["from"].as_str() == Some(&key))
                .map(|e| (e, e["calls"].as_i64().unwrap_or(0).max(0) as u64))
                .collect();
            // Reversed onto a stack, so the busiest child comes off first.
            kids.sort_by_key(|(e, calls)| (*calls, e["to"].as_str().unwrap_or("").to_owned()));
            stack.extend(
                kids.iter()
                    .filter_map(|(e, _)| Some((e["to"].as_str()?.to_owned(), depth + 1))),
            );
        }
        // A cycle, or a service whose only callers were outside the window.
        // Appended flat rather than dropped: a map that silently omits a node
        // reads as "this service is idle".
        for n in nodes {
            if !n["key"].as_str().is_some_and(|k| seen.contains(k)) {
                rows.push((0, n.clone()));
            }
        }
        MapView {
            rows,
            unresolved: y["unresolved"].as_i64().unwrap_or(0),
        }
    }
}

impl Trace {
    fn new(id: String, spans: &[Yaml]) -> Trace {
        let t0 = spans
            .iter()
            .map(|s| i64_of(&s["start_time_unix_nano"]))
            .min()
            .unwrap_or(0);
        let t1 = spans
            .iter()
            .map(|s| i64_of(&s["start_time_unix_nano"]) + i64_of(&s["duration_nano"]))
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
        order.sort_by_key(|&i| i64_of(&spans[i]["start_time_unix_nano"]));

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
    r.put(style, &format!(" {} ", hms(i64_of(&row["time_unix_nano"]))));
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
    let d = i64_of(&row["duration_nano"]);
    let error = row["status_code"].as_i64() == Some(2);
    let mut r = Row::new(w);
    r.put(
        style,
        &format!(" {} ", hms(i64_of(&row["start_time_unix_nano"]))),
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
    let start = i64_of(&s["start_time_unix_nano"]) - t.t0;
    let d = i64_of(&s["duration_nano"]);
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
        ("c", "the frame around this filter: its services and traces"),
        (
            "m",
            "the service map, as a call tree from where traffic arrives",
        ),
        ("f", "follow: re-run the query every 3s and keep the cursor"),
        ("a", "alert rules and what each one is doing right now"),
        (
            "d",
            "node diagnostics: disk, memory, ingest and query counters",
        ),
        ("tab", "metrics: switch between names and series"),
        (
            "esc",
            "back out of a detail, trace, frame, map, alert or help view",
        ),
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
        (
            "",
            "a and d need --addr: they report a process, not a directory",
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

/// A 64-bit integer out of a response, whichever way it was encoded.
///
/// The API renders an int64 as a JSON *string* (section 7.6) — `from`, `to`, `avg_nano`
/// — because 1.7e18 does not survive a double, and a counter small enough to be
/// safe is rendered as a number. Both shapes reach here.
fn i64_of(y: &Yaml) -> i64 {
    y.as_i64()
        .or_else(|| y.as_str()?.parse().ok())
        .unwrap_or_default()
}

/// The filter term that selects one service, for a name picked out of the frame
/// or map pane.
///
/// Always quoted, because [`tokens`] splits on whitespace and a service name is
/// free to contain some. A name carrying a double quote has no equality
/// spelling at all — the tokeniser toggles on one and there is no escape — so it
/// degrades to a `contains` on the part before it, which is a true term rather
/// than one that mis-parses into a different filter.
fn service_term(name: &str) -> String {
    match name.split_once('"') {
        Some((head, _)) => format!("service.name~\"{head}\""),
        None => format!("service.name=\"{name}\""),
    }
}

/// One service in the map: where it sits in the call tree, and what it did.
fn map_row(n: &Yaml, depth: usize, sel: bool, w: usize) -> String {
    let style = if sel { term::REV } else { "" };
    let entry = n["key"].as_str() == Some(ENTRY_KEY);
    let errors = n["errors"].as_i64().unwrap_or(0);
    // The three count columns are 44 wide together, and they are the point of
    // the pane — the name yields to them rather than pushing `avg` off the edge
    // at 80 columns.
    let namew = w.saturating_sub(44).clamp(16, 40);

    let mut r = Row::new(namew);
    // Two columns a level, capped so a deep chain eats its own column rather
    // than pushing the counts off the right-hand edge.
    let indent = "  ".repeat(depth.min(namew / 4));
    r.put(
        match (sel, entry, errors > 0) {
            (true, ..) => style,
            (_, true, _) => term::DIM,
            (_, _, true) => term::RED,
            _ => "",
        },
        &format!(" {indent}{}", n["name"].as_str().unwrap_or("?")),
    );
    r.cap(w).pad_to(namew);
    if !entry {
        r.put(
            term::DIM,
            &format!("{:>9} spans  ", n["spans"].as_i64().unwrap_or(0)),
        );
        match errors {
            // The dash sits in the count column and the word is dropped, so a
            // clean service lines its zero up under the counts above it.
            0 => r.put(term::DIM, &format!("{:>5}{:9}", "-", "")),
            e => r.put(
                if sel { style } else { term::RED },
                &format!("{e:>5} errors  "),
            ),
        };
        r.put(
            term::DIM,
            &format!("{:>9} avg", dur(i64_of(&n["avg_nano"]))),
        );
    }
    r.fill(style)
}

fn num(y: &Yaml) -> Option<f64> {
    match y {
        Yaml::Integer(i) => Some(*i as f64),
        Yaml::Real(r) => r.parse().ok(),
        // An integer point crosses the wire quoted (section 7.6, `Json::i64_str`), so
        // a counter arrives here as a string and a double does not. Refusing
        // the string draws "no points" over a series that has plenty.
        Yaml::String(s) => s.parse().ok(),
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

/// One `label   value` line of the diagnostics pane.
fn kv(w: usize, k: &str, v: &str) -> String {
    let mut r = Row::new(w);
    r.put(term::DIM, &format!("  {k:<16}"));
    r.plain(v);
    r.done()
}

/// An age in seconds, at the precision someone reading it cares about.
///
/// Not [`dur`]: that one formats a span *inside* a request, where the
/// interesting range is nanoseconds to seconds. This one formats how long a
/// process or a block has been around, where it is seconds to days — and 187
/// minutes is not a readable way to say three hours.
fn since(secs: i64) -> String {
    match secs {
        s if s < 0 => "0s".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m {:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {:02}m", s / 3_600, s % 3_600 / 60),
        s => format!("{}d {:02}h", s / 86_400, s % 86_400 / 3_600),
    }
}

/// Binary units, because every other tool an operator has open uses them.
fn bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut b = b;
    for (i, u) in UNITS.iter().enumerate() {
        if b < 1024.0 || i == UNITS.len() - 1 {
            return match i {
                0 => format!("{b:.0} {u}"),
                _ => format!("{b:.1} {u}"),
            };
        }
        b /= 1024.0;
    }
    unreachable!()
}

/// The right-hand side of an alert row: where the number sits against the line.
fn alert_value(a: &Yaml) -> String {
    let (value, threshold) = (
        num(&a["value"]).unwrap_or(0.0),
        num(&a["threshold"]).unwrap_or(0.0),
    );
    let op = a["op"].as_str().unwrap_or(">");
    let head = match a["metric"].as_str() {
        Some("ratio") => format!("{:.2}% {op} {:.2}%", value * 100.0, threshold * 100.0),
        _ => format!("{} {op} {}", tally(value), tally(threshold)),
    };
    let matched = tally(num(&a["matched"]).unwrap_or(0.0));
    match num(&a["total"]) {
        Some(t) => format!("{head}   {matched} of {}", tally(t)),
        None => format!("{head}   {matched} records"),
    }
}

/// A record count. [`g`] is for metric values, where 5 means "about five" and a
/// decimal is information; here it means five records and a decimal is noise.
fn tally(v: f64) -> String {
    match v.abs() < 1e4 {
        true => format!("{v:.0}"),
        false => g(v),
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

        // The word is looked for past the terms, not just at the front: a
        // search that stopped at the first part would report `pod=a`, which is
        // the one term that *was* applied.
        app.filter = "pod=a refused".into();
        assert_eq!(app.ignored_word().as_deref(), Some("refused"));

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

        // The other two spellings a typed value arrives in. An attribute
        // compared against the *string* `"true"` matches nothing at all — OTLP
        // decodes a bool attribute to a bool — and `0.5` truncated to an
        // integer would make `>=0.5` mean `>=0`.
        app.filter = "ok=true ratio>=0.5 span.id=00000000000000042".into();
        let q = crate::api::parse_search(&app.rows_query(), 0).unwrap();
        assert_eq!(q.terms[0].value, Value::Bool(true));
        assert_eq!(q.terms[1].value, Value::Double(0.5));
        // ...and an id made entirely of digits is still an id.
        assert!(matches!(&q.terms[2].value, Value::Str(_)));
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
        // Every scalar an OTLP attribute can decode to, each printed as itself.
        assert_eq!(v(r#"{"v":[1.5,true,null]}"#), "[1.5, true, null]");
        // A hash at the cap is summarised by size, the same as an array is.
        assert_eq!(v(r#"{"v":[[[[{"a":1,"b":2}]]]]}"#), "[[[[{2 keys}]]]]");
        // A key the response does not carry at all. The loader answers
        // `BadValue`, and this pane prints what it is handed — so the empty
        // string is what keeps `BadValue` off the screen.
        assert_eq!(v("{}"), "");
    }

    /// The number formatters, each at the boundary it exists for.
    ///
    /// They are pure and tiny, and every one of them is a place where a wrong
    /// unit reads as a right answer — `1.83` where `1.83 ms` was meant is the
    /// whole story of a diagnostics pane nobody trusts.
    #[test]
    fn each_formatter_covers_the_range_it_was_written_for() {
        // `dur` measures a span inside a request: nanoseconds to minutes.
        assert_eq!(dur(940), "940ns");
        assert_eq!(dur(1_500), "1.5µs");
        assert_eq!(dur(2_500_000), "2.50ms");
        assert_eq!(dur(1_250_000_000), "1.25s");
        assert_eq!(dur(90_000_000_000), "1m30s");

        // `since` measures how long something has been around: seconds to days.
        assert_eq!(since(-1), "0s");
        assert_eq!(since(41), "41s");
        assert_eq!(since(125), "2m 05s");
        assert_eq!(since(7_384), "2h 03m");
        assert_eq!(since(93_784), "1d 02h");

        assert_eq!(ms(std::time::Duration::from_micros(1_830)), "1.8ms");
        assert_eq!(ms(std::time::Duration::from_millis(2_500)), "2.50s");

        // A metric value: counters in the millions and ratios below one share
        // one column, so neither may be printed in the other's format.
        assert_eq!(g(0.0), "0");
        assert_eq!(g(0.0481), "0.048");
        assert_eq!(g(12.5), "12.5");
        assert_eq!(g(12_040.0), "12.0k");
        assert_eq!(g(1_204_000.0), "1.2M");

        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KiB");
        assert_eq!(bytes(1.5 * 1024f64.powi(4)), "1.5 TiB");
        // Past the last unit there is nothing left to divide by, and the answer
        // is a big number rather than a panic.
        assert_eq!(bytes(4096.0 * 1024f64.powi(4)), "4096.0 TiB");

        // The OTLP `SpanKind` enum. Zero and out-of-range are the same answer,
        // because the spec's own zero *is* unspecified.
        let kinds: Vec<&str> = (0i64..7).map(kind).collect();
        assert_eq!(
            kinds,
            [
                "unspecified",
                "internal",
                "server",
                "client",
                "producer",
                "consumer",
                "unspecified",
            ]
        );

        // Severity bands are the OTLP numbers, not a guess at `severity_text`.
        assert_eq!(sev_style(21), term::RED);
        assert_eq!(sev_style(13), term::YELLOW);
        assert_eq!(sev_style(9), term::GREEN);
        assert_eq!(sev_style(1), term::DIM);
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
            // Quoted, as the server writes them (section 7.6).
            r#"{"rows":[
              {"span_id":"02","parent_span_id":"01","start_time_unix_nano":"30","duration_nano":"5"},
              {"span_id":"01","start_time_unix_nano":"10","duration_nano":"100"},
              {"span_id":"03","parent_span_id":"01","start_time_unix_nano":"20","duration_nano":"5"},
              {"span_id":"04","parent_span_id":"03","start_time_unix_nano":"21","duration_nano":"1"},
              {"span_id":"09","parent_span_id":"ff","start_time_unix_nano":"90","duration_nano":"1"}
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

    /// Every 64-bit field crosses the wire as a JSON string (section 7.6), and
    /// `Yaml::as_i64` says `None` to a string. Each `unwrap_or(0)` behind one
    /// of those reads is therefore a pane that renders — clock at the epoch,
    /// bars of zero width, `no points` over a full series — rather than a pane
    /// that fails, which is why this is asserted against the wire spelling and
    /// not against the shape a fixture finds convenient.
    #[test]
    fn a_quoted_64_bit_field_still_reaches_the_screen() {
        let row = &crate::api::parse(
            r#"{"time_unix_nano":"1788877362987743417","severity_text":"INFO",
                "start_time_unix_nano":"1788877362987743417","duration_nano":"5000000",
                "name":"GET /x","body":"hello"}"#,
        )
        .unwrap();

        // Not 00:00:00 in UTC nor 01:00:00 in CET — the epoch under either.
        let clock = strip(&log_row(row, 60, false));
        assert!(!clock.contains(&hms(0)), "{clock:?}");
        assert!(clock.contains(&hms(1_788_877_362_987_743_417)), "{clock:?}");

        // A bar drawn against a scale it is the whole of is a full bar.
        let span = strip(&span_row(row, 90, false, i64_of(&row["duration_nano"])));
        assert!(span.contains("5.00ms"), "{span:?}");
        assert!(span.matches('▂').count() > 1, "{span:?}");

        // An integer point is quoted; a double is not. Both chart.
        assert_eq!(num(&Yaml::String("350".into())), Some(350.0));
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.series = array(
            &crate::api::parse(
                r#"{"series":[{"name":"m","attributes":{},"points":[["1","10"],["2","0"]]}]}"#,
            )
            .unwrap()["series"],
        );
        let chart = strip(&app.series_lines(80)[1]);
        assert!(!chart.contains("no points"), "{chart:?}");
        assert!(chart.contains('█'), "{chart:?}");
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

    /// The map is a graph on the wire and a tree on screen, and every way that
    /// flattening can go wrong loses a service silently.
    #[test]
    fn the_map_flattens_to_a_tree_without_dropping_a_service() {
        let node =
            |k: &str, n: &str| format!(r#"{{"key":"{k}","name":"{n}","spans":1,"errors":0}}"#);
        let edge = |a: &str, b: &str, c: u64| format!(r#"{{"from":"{a}","to":"{b}","calls":{c}}}"#);
        let map = |nodes: &[String], edges: &[String]| {
            let doc = format!(
                r#"{{"nodes":[{}],"edges":[{}],"unresolved":7}}"#,
                nodes.join(","),
                edges.join(",")
            );
            MapView::new(&crate::api::parse(&doc).unwrap())
        };
        let names = |m: &MapView| -> Vec<(usize, String)> {
            m.rows
                .iter()
                .map(|(d, n)| (*d, n["name"].as_str().unwrap_or("?").to_owned()))
                .collect()
        };

        // Busiest branch first, because the hot path is what a map is opened
        // for. `entry` is on top even though it is not one of the nodes.
        let m = map(
            &[node("1", "gw"), node("2", "slow"), node("3", "hot")],
            &[
                edge("entry", "1", 10),
                edge("1", "2", 1),
                edge("1", "3", 99),
            ],
        );
        assert_eq!(
            names(&m),
            [
                (0, "entry".into()),
                (1, "gw".into()),
                (2, "hot".into()),
                (2, "slow".into())
            ]
        );
        assert_eq!(m.unresolved, 7);

        // A retry loop. Drawn once, at the depth it was first reached — a tree
        // that re-expands a cycle does not terminate.
        let m = map(
            &[node("1", "a"), node("2", "b")],
            &[edge("entry", "1", 1), edge("1", "2", 1), edge("2", "1", 1)],
        );
        assert_eq!(
            names(&m),
            [(0, "entry".into()), (1, "a".into()), (2, "b".into())]
        );

        // A service whose only callers were outside the window is unreachable
        // from `entry`. It goes on the end flat rather than vanishing: a map
        // that omits a node reads as "this service is idle".
        let m = map(
            &[node("1", "a"), node("9", "orphan")],
            &[edge("entry", "1", 1)],
        );
        assert_eq!(
            names(&m),
            [(0, "entry".into()), (1, "a".into()), (0, "orphan".into())]
        );

        // The span budget can cut a sample mid-trace, so an edge can name a
        // node that is not in `nodes` at all.
        let m = map(
            &[node("1", "a")],
            &[edge("entry", "1", 1), edge("1", "404", 1)],
        );
        assert_eq!(names(&m), [(0, "entry".into()), (1, "a".into())]);

        // Nothing at all: `Job::Map` reports it rather than opening the pane,
        // and the header's `len() - 1` must not underflow if it ever does.
        assert!(map(&[], &[]).rows.is_empty());
    }

    /// What the two new panes do with Enter. Everything either pane offers has
    /// to land back in an ordinary query — that is the point of the algebra's
    /// closure, and a selection that goes nowhere is the way to lose it.
    #[test]
    fn every_line_of_the_frame_and_map_leads_back_into_a_query() {
        let doc = crate::api::parse(
            r#"{"frame":{"from":"1000","to":"5000","truncated":false,
                 "entities":[{"key":"7","name":"checkout"},{"key":"8","name":"checkout"},
                             {"key":"9","name":"api"}],
                 "traces":["abababababababababababababababab"]},
               "map":{"nodes":[{"key":"7","name":"checkout","spans":2,"errors":1,
                                "avg_nano":"3000"}],
                 "edges":[{"from":"entry","to":"7","calls":2}],"unresolved":0}}"#,
        )
        .unwrap();
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.frame = Some(FrameView::new(&doc["frame"]));
        app.map = Some(MapView::new(&doc["map"]));

        // Two entity keys, one name: an entity is an instance (section 7.2), and the
        // count is the only thing that says a service has replicas. Sorted by
        // name, because the engine sorts by key and a key is a hash.
        let f = app.frame.as_ref().unwrap();
        assert_eq!(f.services, [("api".into(), 1), ("checkout".into(), 2)]);
        assert_eq!(f.len(), 3);

        app.mode = Mode::Frame;
        app.psel = 1;
        assert!(matches!(app.pick(), Some(Pick::Service(s)) if s == "checkout"));
        // Past the services, the same cursor indexes the traces.
        app.psel = 2;
        assert!(matches!(app.pick(), Some(Pick::Trace(t)) if t.starts_with("abab")));
        app.follow_pick();
        assert!(matches!(app.job, Some(Job::Trace(_))));
        // And `t` follows the pane's own selection, not the list behind it.
        app.job = None;
        app.open_trace();
        assert!(matches!(app.job, Some(Job::Trace(_))));

        // A service is anded onto the filter and hands the reader back to the
        // list, so the next thing they do is an ordinary query.
        app.psel = 0;
        app.filter = "body~timeout".into();
        app.follow_pick();
        assert_eq!(app.filter, r#"body~timeout service.name="api""#);
        assert_eq!(app.mode, Mode::List);
        // Twice is a double-press.
        app.mode = Mode::Frame;
        app.follow_pick();
        assert_eq!(app.filter, r#"body~timeout service.name="api""#);

        // `entry` is the synthetic caller of every root span, so there is no
        // service behind it to filter on and Enter must do nothing.
        app.mode = Mode::Map;
        app.psel = 0;
        assert!(app.pick().is_none());
        app.psel = 1;
        assert!(matches!(app.pick(), Some(Pick::Service(s)) if s == "checkout"));

        // Only those two panes have a pick at all. The record list's cursor is
        // still set behind them, and answering with *that* row's service would
        // make Enter act on something nobody is pointing at.
        for mode in [Mode::List, Mode::Detail, Mode::Trace, Mode::Alerts] {
            app.mode = mode;
            assert!(app.pick().is_none(), "{mode:?}");
        }

        // The filter box splits on whitespace and toggles on `"`, so a name
        // carrying either has to survive the round trip into a real term.
        assert_eq!(service_term("a b"), r#"service.name="a b""#);
        assert_eq!(
            parse_filter(&service_term("a b")),
            [term("service.name", "eq", "a b")]
        );
        // No equality spelling for an embedded quote; the prefix is a true
        // term. `parse_filter` trims it, so it is `say` rather than `say `.
        assert_eq!(
            parse_filter(&service_term(r#"say "hi""#)),
            [term("service.name", "contains", "say")]
        );
    }

    /// The two node panes, from the keystroke to the pixels.
    ///
    /// A layout test proves nothing here: `Row` clips at `max` rather than
    /// overflowing, so a pane that has silently lost its numbers still passes
    /// `every_view_fits_the_frame_at_any_size` with the right line count. This
    /// one reads the numbers back off the screen.
    #[test]
    fn the_node_panes_show_the_numbers_the_endpoints_answered() {
        const ROWS: &str = r#"{"rows":[],"stats":{"blocks_total":0,"blocks_scanned":0,
                               "rows_scanned":0,"rows_matched":0}}"#;
        const ALERTS: &str = r#"{"alerts":[
            {"name":"checkout-error-rate","state":"firing","severity":"critical",
             "metric":"ratio","op":">","threshold":0.05,"value":0.5,"matched":5,"total":10,
             "over_nano":"60000000000","for_nano":"0","since":"1","firing_since":"1",
             "evaluated_at":"2","signal":"traces",
             "filter":"attr:service.name=checkout field:status_code=2",
             "link":"https://m/#/traces?q=x","error":null},
            {"name":"quiet","state":"ok","severity":"warning","metric":"count","op":">=",
             "threshold":1.0,"value":0.0,"matched":0,"total":null,"over_nano":"300000000000",
             "for_nano":"0","since":null,"firing_since":null,"evaluated_at":"2",
             "signal":"logs","filter":"","link":"","error":null}],
            "every_nano":"5000000000"}"#;
        const STATS: &str = r#"{"uptime_s":93784,"peak_rss_bytes":441450496,
            "free_fraction":0.07,"queries":{"count":12040,"mean_ms":1.83,"max_ms":412.5},
            "signals":{"logs":{"shed":3,"failed":0,"refused":1,"blocks_published":71,
              "rows":1204000,"bytes":158000000,"blocks_on_disk":69,"open_block_age_s":12,
              "stalled_s":41}}}"#;

        // One reply per connection, in the order this session asks: the first
        // list query, `a`, the requery Enter fires, then `d`.
        let addr = source::serve([ROWS, ALERTS, ROWS, STATS].map(source::ok).to_vec());
        let mut app = App::new(Source::Remote(addr));
        settle(&mut app);

        let a = strip(&press(&mut app, Key::Char('a')));
        assert_eq!(app.mode, Mode::Alerts);
        assert!(a.contains("2 rules"), "{a}");
        assert!(a.contains("1 firing"), "{a}");
        // The ratio reads as a percentage against its threshold, with the two
        // counts behind it — the whole of why the rule fired, on one line.
        assert!(a.contains("50.00% > 5.00%"), "{a}");
        assert!(a.contains("5 of 10"), "{a}");
        // A count rule has no denominator and must not invent one.
        assert!(a.contains("0 records") && !a.contains("0 of 0"), "{a}");
        assert!(
            a.contains("attr:service.name=checkout field:status_code=2"),
            "{a}"
        );
        assert!(a.contains("(no filter — every record)"), "{a}");
        assert!(a.contains("over 1m00s"), "{a}");

        // Enter lands on the rows the rule counted: its signal's tab, its terms
        // in the filter box, and a query already in flight.
        press(&mut app, Key::Enter);
        assert_eq!(app.tab, Tab::Traces);
        assert_eq!(app.filter, "attr:service.name=checkout field:status_code=2");
        assert_eq!(app.mode, Mode::List);

        let d = strip(&press(&mut app, Key::Char('d')));
        assert_eq!(app.mode, Mode::Diag);
        assert!(d.contains("up 1d 02h"), "{d}");
        assert!(d.contains("peak rss 421.0 MiB"), "{d}");
        assert!(d.contains("disk 7% free"), "{d}");
        assert!(d.contains("12.0k") && d.contains("1.83 ms"), "{d}");
        assert!(
            d.contains("69 on disk") && d.contains("71 published"),
            "{d}"
        );
        assert!(
            d.contains("1.2M"),
            "a million rows is not printed in full: {d}"
        );
        // The cost-per-GB axis on this node's own data: 158 MB over 1.204M rows.
        assert!(d.contains("150.7 MiB") && d.contains("131 B/row"), "{d}");
        assert!(d.contains("3 shed") && d.contains("1 refused"), "{d}");
        // Stalled is the readiness condition; it is never left off when set.
        assert!(d.contains("cannot store this signal"), "{d}");
        // A signal the document did not mention is skipped, not drawn empty —
        // its section rule is what is absent, not the word, which is also a tab.
        assert!(!d.contains("── metrics") && !d.contains("── traces"), "{d}");
    }

    /// Both panes report a process. A directory is not one, and saying so is
    /// better than an empty screen that looks like "no rules, all healthy".
    #[test]
    fn the_node_panes_refuse_a_directory_by_name() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.job = None;
        for (k, want) in [('a', "alerts"), ('d', "stats")] {
            let f = strip(&press(&mut app, Key::Char(k)));
            assert!(app.err, "{k} should have failed");
            assert!(f.contains(want) && f.contains("--addr"), "{f}");
            assert_eq!(app.mode, Mode::List, "and must not open the pane");
        }
    }

    /// Painting must never panic and never overrun, whatever the terminal size
    /// or the mode — a panic here leaves the user's shell in raw mode.
    #[test]
    fn every_view_fits_the_frame_at_any_size() {
        let doc = crate::api::parse(
            // Every 64-bit field is quoted, because that is how it arrives:
            // `query.rs` writes `Int64`, `UInt64` and `Timestamp` through
            // `Json::i64_str` (section 7.6). A fixture that spells them bare tests a
            // response the server never sends.
            r#"{"rows":[{"time_unix_nano":"1788877362987743417","severity_number":17,
                 "severity_text":"ERROR","body":"boom","duration_nano":"5000000",
                 "status_code":2,"name":"GET /x","span_id":"01",
                 "trace_id":"abababababababababababababababab",
                 "events":[{"name":"ev","time_unix_nano":"1788877362987743500"}],
                 "links":[{"trace_id":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}],
                 "attributes":{"service.name":"checkout"}}],
               "series":[{"name":"m","attributes":{"service.name":"c"},
                 "points":[["1","2"],["2","3"]],
                 "exemplars":[{"trace_id":"abababababababababababababababab"}]}],
               "names":[{"name":"http.server.duration","unit":"ms","kind":"histogram"}],
               "frame":{"from":"1788877362987743417","to":"1788877372987743417",
                 "entities":[{"key":"1","name":"checkout"},{"key":"2","name":"checkout"},
                             {"key":"3","name":"a-very-long-service-name-indeed"}],
                 "traces":["abababababababababababababababab"],"truncated":true},
               "map":{"nodes":[{"key":"1","name":"frontend","spans":9,"errors":0,
                                "avg_nano":"1000000"},
                               {"key":"2","name":"checkout","spans":4,"errors":2,
                                "avg_nano":"22000000"}],
                 "edges":[{"from":"entry","to":"1","calls":9,"errors":0,
                           "avg_nano":"1000000","max_nano":"2000000"},
                          {"from":"1","to":"2","calls":4,"errors":2,
                           "avg_nano":"22000000","max_nano":"90000000"}],
                 "unresolved":3},
               "alerts":[{"name":"checkout-error-rate","state":"firing","severity":"critical",
                 "metric":"ratio","op":">","threshold":0.05,"value":0.5,"matched":5,"total":10,
                 "over_nano":"60000000000","for_nano":"0","since":"1788877362987743417",
                 "firing_since":"1788877362987743417","evaluated_at":"1788877372987743417",
                 "signal":"logs","filter":"attr:service.name=checkout field:severity_number>=17",
                 "link":"https://m/#/logs?q=x","error":null},
                {"name":"a-rule-whose-name-is-far-too-long-to-fit-in-any-column","state":"ok",
                 "severity":"warning","metric":"count","op":">=","threshold":1.0,"value":0.0,
                 "matched":0,"total":null,"over_nano":"300000000000","for_nano":"120000000000",
                 "since":null,"firing_since":null,"evaluated_at":"1788877372987743417",
                 "signal":"traces","filter":"","link":"","error":"block directory unreadable"}]}"#,
        )
        .unwrap();
        // The diagnostics pane renders the stats document as it arrives, so the
        // fixture is one: absent counters are `null` there, never zero.
        let diag = crate::api::parse(
            r#"{"uptime_s":93784,"peak_rss_bytes":441450496,"free_fraction":0.07,
                "queries":{"count":12040,"mean_ms":1.83,"max_ms":412.5},
                "signals":{"logs":{"shed":3,"failed":0,"refused":1,"blocks_published":71,
                             "rows":1204000,"bytes":158000000,"blocks_on_disk":69,
                             "open_block_age_s":12,"stalled_s":41},
                           "traces":{"shed":0,"failed":0,"refused":0,"blocks_published":0,
                             "rows":0,"bytes":0,"blocks_on_disk":null,
                             "open_block_age_s":null,"stalled_s":null}}}"#,
        )
        .unwrap();
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.rows = array(&doc["rows"]);
        app.series = array(&doc["series"]);
        app.names = array(&doc["names"]);
        app.trace = Some(Trace::new("ab".into(), &array(&doc["rows"])));
        app.frame = Some(FrameView::new(&doc["frame"]));
        app.map = Some(MapView::new(&doc["map"]));
        app.alerts = array(&doc["alerts"]);
        app.diag = Some(diag);
        app.stats = "x".into();

        for (w, h) in [(40, 8), (80, 24), (200, 60), (41, 9)] {
            for tab in [Tab::Logs, Tab::Traces, Tab::Metrics] {
                for mode in [
                    Mode::List,
                    Mode::Filter,
                    Mode::Detail,
                    Mode::Trace,
                    Mode::Span,
                    Mode::Frame,
                    Mode::Map,
                    Mode::Alerts,
                    Mode::Diag,
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
        mira_core::block::publish(&dir, "logs", node, 0, 0, &b.finish().unwrap()).unwrap();

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
        mira_core::block::publish(&dir, "traces", node, 0, 0, &b.finish().unwrap()).unwrap();

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
        mira_core::block::publish(&dir, "metrics", node, 0, 0, &b.finish().unwrap()).unwrap();
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
        // Bounded so a job that queues itself fails the test instead of hanging
        // it. Eight is three more than the longest real sequence.
        let mut left = 8;
        while let Some(job) = app.job.take() {
            assert!(left > 0, "a job kept queueing another one");
            left -= 1;
            app.run(job, H);
            f = app.frame(W, H);
        }
        f.join("\n")
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

    /// The two panes built out of the frame algebra, reached the way a reader
    /// reaches them.
    ///
    /// `every_line_of_the_frame_and_map_leads_back_into_a_query` drives `pick`
    /// against a fixture, which proves what Enter means but not that either key
    /// sends a document the engine answers. This one presses the keys against a
    /// real block directory, so the query is the one `/api/v1/correlate` and
    /// `/api/v1/map` actually run.
    #[test]
    fn the_frame_and_map_panes_open_from_a_keystroke() {
        let dir = store("panes");
        let mut app = App::new(Source::Local(dir));
        settle(&mut app);

        // Metrics is not an anchor: a frame widens out from records and a
        // series is not one. Said, rather than sent and returned empty.
        press(&mut app, Key::Char('3'));
        let f = strip(&press(&mut app, Key::Char('c')));
        assert!(app.err, "{f}");
        assert!(f.contains("switch tab first"), "{f}");
        assert_eq!(app.mode, Mode::List);

        // `h` walks the tabs the way `l` does not, and a digit jumps.
        press(&mut app, Key::Char('h'));
        assert_eq!(app.tab, Tab::Traces);
        press(&mut app, Key::Char('h'));
        assert_eq!(app.tab, Tab::Logs);
        press(&mut app, Key::Char('1'));
        assert_eq!(app.tab, Tab::Logs, "the tab already on is not a reload");
        press(&mut app, Key::Char('2'));
        assert_eq!(app.tab, Tab::Traces);

        let f = strip(&press(&mut app, Key::Char('c')));
        assert_eq!(app.mode, Mode::Frame);
        assert!(f.contains("services") && f.contains("checkout"), "{f}");
        assert!(f.contains("traces"), "{f}");

        // The pane owns its own cursor, and it clamps like every other one.
        let last = app.frame.as_ref().unwrap().len() - 1;
        press(&mut app, Key::Char('G'));
        assert_eq!(app.psel, last);
        press(&mut app, Key::Char('j'));
        assert_eq!(
            app.psel, last,
            "clamped at the end of the pane, not the list"
        );
        press(&mut app, Key::Char('k'));
        press(&mut app, Key::Char('g'));
        assert_eq!(app.psel, 0);

        // No services on the frame: `peers` reports who the anchor set talks
        // *to*, and this corpus is one service talking to itself. So the whole
        // pane is its traces, and Enter on one opens the waterfall.
        assert!(app.frame.as_ref().unwrap().services.is_empty());
        let f = strip(&press(&mut app, Key::Enter));
        assert_eq!(app.mode, Mode::Trace, "{f}");
        press(&mut app, Key::Char('q'));
        assert_eq!(app.mode, Mode::List);

        // The map runs over the same window and does have a service in it: it
        // is built from `parent_span_id` at read time, so one service calling
        // itself is still an edge.
        let f = strip(&press(&mut app, Key::Char('m')));
        assert_eq!(app.mode, Mode::Map);
        assert!(f.contains("map"), "{f}");

        // The cursor starts on `entry`, the synthetic caller of every root
        // span. There is no service behind it and no trace under it, so both
        // keys that act on a selection have to decline rather than act on the
        // row below or on the record list behind the pane.
        assert_eq!(app.psel, 0);
        press(&mut app, Key::Enter);
        assert_eq!(app.mode, Mode::Map, "enter on entry opens nothing");
        assert!(
            app.filter.is_empty(),
            "and filters on nothing: {}",
            app.filter
        );
        let f = strip(&press(&mut app, Key::Char('t')));
        assert_eq!(app.mode, Mode::Map, "{f}");
        assert!(f.contains("nothing here carries a trace id"), "{f}");

        // Off `entry`, which is synthetic, and onto the service under it.
        press(&mut app, Key::Char('j'));
        press(&mut app, Key::Enter);
        assert_eq!(app.mode, Mode::List);
        assert!(app.filter.contains("service.name="), "{}", app.filter);

        // Follow belongs to the record list, and the bar says when it is on.
        app.filter.clear();
        let f = strip(&press(&mut app, Key::Char('f')));
        assert!(app.tail, "{f}");
        assert!(f.contains("follow"), "{f}");
        press(&mut app, Key::Char('f'));
        assert!(!app.tail);
        press(&mut app, Key::Char('3'));
        let f = strip(&press(&mut app, Key::Char('f')));
        assert!(app.err, "{f}");
        assert!(f.contains("logs or traces list"), "{f}");

        // Nothing matched. The frame does not open on an empty answer: an empty
        // frame drawn as a frame reads as "these are all the services".
        press(&mut app, Key::Char('2'));
        app.filter = "service.name=nosuchservice".into();
        let f = strip(&press(&mut app, Key::Char('c')));
        assert_eq!(app.mode, Mode::List, "{f}");
        assert!(f.contains("no frame to widen"), "{f}");

        // The map answers for the window and not for the filter — `/api/v1/map`
        // takes no `where` — so the only way to empty it is an empty store. Not
        // an error: a directory with logs and no spans is an ordinary one.
        let empty = std::env::temp_dir().join("mira-tui-nomap");
        std::fs::create_dir_all(&empty).unwrap();
        let mut app = App::new(Source::Local(empty));
        settle(&mut app);
        let f = strip(&press(&mut app, Key::Char('m')));
        assert_eq!(app.mode, Mode::List, "{f}");
        assert!(f.contains("there is no map"), "{f}");

        // And the metrics tab over the same empty store. An empty name list is
        // not an error either, but it has to say which of the two it is: the
        // pane otherwise sits blank next to a chart area, which reads as a
        // metric that failed to load rather than a window with no metrics in it.
        let f = strip(&press(&mut app, Key::Char('3')));
        assert_eq!(app.tab, Tab::Metrics);
        assert!(f.contains("no metrics in this window"), "{f}");
        assert!(app.names.is_empty() && app.series.is_empty(), "{f}");
    }

    /// A mode outlives the data behind it: the query that would have filled the
    /// pane failed, or the pane was open when the store went away. The frame
    /// draws whatever mode is set, so each pane checks its own `Option` — and
    /// the failure this prevents is the previous pane's rows drawn under the
    /// new pane's header, which is a screenful of numbers about something else.
    #[test]
    fn a_pane_whose_data_never_arrived_draws_nothing_rather_than_something_else() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.job = None;
        app.stats = "0/0 blocks".into();
        for (mode, header) in [
            (Mode::Frame, " frame "),
            (Mode::Map, " map "),
            (Mode::Diag, " node "),
            (Mode::Trace, " trace "),
        ] {
            app.mode = mode;
            let f = app.frame(W, H);
            assert_eq!(f.len(), H, "{mode:?} still owes the terminal every row");
            let text = strip(&f.join("\n"));
            assert!(!text.contains(header), "{mode:?} drew a header: {text}");
        }
        // The detail pane says so instead of drawing nothing, because it is the
        // one pane opened *on* a selection — blank there reads as a record with
        // no fields rather than as no record.
        app.mode = Mode::Detail;
        let text = strip(&app.frame(W, H).join("\n"));
        assert!(text.contains("nothing selected"), "{text}");
    }

    /// The one number on the node pane that is a reason to wake someone up, and
    /// the colour is the whole of the signal: a node with 7% of its disk left
    /// drawn in the same grey as one with 90% is a page nobody makes.
    ///
    /// Both boundaries are in the table, because `<` and `<=` are one keystroke
    /// apart and only a value sitting exactly on 0.1 or 0.2 tells them apart.
    /// Zero is there because a full disk is the case the colour exists for, and
    /// it is the one an operator sees at the worst possible moment.
    #[test]
    fn the_disk_line_is_coloured_by_how_close_the_node_is_to_full() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        for (doc, style, text) in [
            (r#"{"free_fraction":0.0}"#, term::RED, "disk 0% free"),
            (r#"{"free_fraction":0.07}"#, term::RED, "disk 7% free"),
            (r#"{"free_fraction":0.1}"#, term::YELLOW, "disk 10% free"),
            (r#"{"free_fraction":0.15}"#, term::YELLOW, "disk 15% free"),
            (r#"{"free_fraction":0.2}"#, term::DIM, "disk 20% free"),
            (r#"{"free_fraction":0.9}"#, term::DIM, "disk 90% free"),
            // Absent is neither full nor empty: `statfs` would not answer, and
            // a percentage invented for that case is the one number here that
            // must never be made up.
            (r#"{"free_fraction":null}"#, term::DIM, "disk unreadable"),
        ] {
            app.diag = Some(crate::api::parse(doc).unwrap());
            let head = app.diag_pane(W).remove(0);
            assert!(strip(&head).contains(text), "{head:?}");
            assert!(head.contains(&format!("{style}{text}")), "{head:?}");
        }
    }

    /// A volume with no write barrier is a durability fact, and the header is
    /// where a node's durability facts live. It is absent at zero on purpose:
    /// a line that reads "0" on every healthy node is a line the eye learns to
    /// skip, which is exactly the wrong training for the one node it matters on.
    #[test]
    fn degraded_syncs_appear_only_once_there_are_some() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        for (doc, want) in [
            (r#"{"degraded_syncs":0}"#, None),
            (r#"{}"#, None),
            (r#"{"degraded_syncs":1}"#, Some("degraded syncs 1")),
            (r#"{"degraded_syncs":41000}"#, Some("degraded syncs 41.0k")),
        ] {
            app.diag = Some(crate::api::parse(doc).unwrap());
            let head = app.diag_pane(W).remove(0);
            match want {
                Some(text) => {
                    assert!(strip(&head).contains(text), "{head:?}");
                    assert!(
                        head.contains(&format!("{}  ·  {text}", term::YELLOW)),
                        "{head:?}"
                    );
                }
                None => assert!(!strip(&head).contains("degraded"), "{head:?}"),
            }
        }
    }

    /// A series the engine answered with no points in the window. An empty
    /// sparkline and a flat one at zero are the same picture and opposite
    /// facts — nothing reported, versus reported as nothing.
    #[test]
    fn a_series_with_no_points_says_so_rather_than_drawing_a_flat_line() {
        let mut app = App::new(Source::Local("/nonexistent".into()));
        app.series = array(
            &crate::api::parse(
                r#"{"series":[{"name":"m","attributes":{"pod":"a"},"points":[],
                              "exemplars":[]}]}"#,
            )
            .unwrap()["series"],
        );
        let lines = app.series_lines(60);
        let chart = strip(&lines[1]);
        assert!(chart.contains("no points"), "{chart:?}");
        assert!(!chart.contains('▁') && !chart.contains('█'), "{chart:?}");
        // The three-line shape holds anyway, or the caller's `ssel * 3` window
        // lands on the wrong series.
        assert_eq!(lines.len(), 3);
        assert!(strip(&lines[0]).contains("pod=a"), "{:?}", lines[0]);
    }

    /// A failed span is red in both places it is drawn — and is not, under the
    /// cursor, because red on reverse-video is the one combination that
    /// disappears on a light terminal. The badge stays either way: colour is
    /// how the eye finds the row, the word is how the reader confirms it.
    #[test]
    fn a_failed_span_is_red_in_the_list_and_the_waterfall_unless_it_is_selected() {
        let rows = array(
            &crate::api::parse(
                r#"{"rows":[
                 {"span_id":"01","name":"GET /ok","start_time_unix_nano":"10",
                  "duration_nano":"1000000","status_code":1},
                 {"span_id":"02","parent_span_id":"01","name":"POST /pay",
                  "start_time_unix_nano":"20","duration_nano":"2000000","status_code":2}]}"#,
            )
            .unwrap()["rows"],
        );
        let (ok, bad) = (&rows[0], &rows[1]);

        let line = span_row(bad, W, false, 2_000_000);
        assert!(line.contains(&format!("{}POST", term::RED)), "{line:?}");
        assert!(strip(&line).contains("ERROR"), "{line:?}");
        let sel = span_row(bad, W, true, 2_000_000);
        assert!(!sel.contains(term::RED), "{sel:?}");
        assert!(strip(&sel).contains("ERROR"), "{sel:?}");
        let fine = span_row(ok, W, false, 2_000_000);
        assert!(!fine.contains(term::RED), "{fine:?}");
        assert!(!strip(&fine).contains("ERROR"), "{fine:?}");

        // The waterfall, whose selection is its own and whose bar takes the
        // same colour as the name — a green bar on a failed span would be the
        // pane contradicting itself.
        let t = Trace::new("ab".into(), &rows);
        let ends = [16, 24, W - 11, W];
        let bar = span_bar(bad, 1, false, ends, &t);
        assert!(bar.starts_with(term::RED), "{bar:?}");
        assert!(strip(&bar).contains("POST /pay"), "{bar:?}");
        assert!(bar.contains(&format!("{}█", term::RED)), "{bar:?}");
        let bar = span_bar(ok, 0, false, ends, &t);
        assert!(!bar.contains(term::RED), "{bar:?}");
        assert!(bar.contains(&format!("{}█", term::GREEN)), "{bar:?}");
    }

    /// An event's own attributes are the payload of the event — an exception's
    /// type and stacktrace live nowhere else — and the detail pane is the only
    /// screen that ever shows them. Rendering the event and dropping them
    /// throws away the reason the reader opened the record.
    #[test]
    fn an_events_own_attributes_are_indented_under_it_in_the_detail_pane() {
        let row = crate::api::parse(
            r#"{"body":"boom","attributes":{"service.name":"checkout"},
                "events":[{"name":"exception","time_unix_nano":"10",
                           "attributes":{"exception.type":"IOError"}}],
                "links":[{"trace_id":"abab","attributes":{"rel":"follows"}}]}"#,
        )
        .unwrap();
        let text = detail(&row, 80)
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("exception.type"), "{text}");
        assert!(text.contains("IOError"), "{text}");
        // Indented one level further than the event's own fields, so a reader
        // can tell whose attribute it is.
        assert!(text.contains("    rel "), "{text}");
        assert!(text.contains("follows"), "{text}");
        // And rendered as lines rather than as one inline map next to the key,
        // which is what the top-level `attributes` skip is there to prevent.
        assert!(!text.contains("{exception.type"), "{text}");
    }

    /// Two lists, one set of arrow keys, and the keys that do nothing at all.
    ///
    /// Every `_ => {}` in the key handler exists so a stray keystroke is not a
    /// quit, a reload or a mode change — the arms around each one all have side
    /// effects. The cursor arms are the other half: the metrics tab keeps two
    /// indices, and moving the wrong one scrolls a pane nobody is looking at.
    #[test]
    fn an_unbound_key_changes_nothing_and_each_pane_moves_its_own_cursor() {
        let dir = store("keys");
        let mut app = App::new(Source::Local(dir));
        settle(&mut app);

        // The tab ring closes in both directions; these are the two arms the
        // walk in `the_frame_and_map_panes_open_from_a_keystroke` never reaches.
        press(&mut app, Key::Char('h'));
        assert_eq!(app.tab, Tab::Metrics, "h off the left of the ring wraps");
        press(&mut app, Key::Char('l'));
        assert_eq!(app.tab, Tab::Logs, "and l off the right wraps back");

        // A key with no binding leaves the screen byte for byte as it was.
        let before = strip(&app.frame(W, H).join("\n"));
        for k in [Key::BackTab, Key::Char('z'), Key::Char('#')] {
            let f = strip(&press(&mut app, k));
            assert_eq!(f, before, "{k:?} changed the screen");
        }

        // Enter in a pane with nothing to open is one of them.
        press(&mut app, Key::Char('?'));
        assert_eq!(app.mode, Mode::Help);
        press(&mut app, Key::Enter);
        assert_eq!(app.mode, Mode::Help, "enter in help opens nothing");
        press(&mut app, Key::Esc);

        // In the filter box, so is every key that is neither text nor an edit:
        // an arrow key typed into the filter would be a query for `\x1b[A`.
        press(&mut app, Key::Char('/'));
        typed(&mut app, "abc");
        press(&mut app, Key::Up);
        press(&mut app, Key::PageDown);
        assert_eq!(app.filter, "abc");
        assert_eq!(app.mode, Mode::Filter);
        press(&mut app, Key::Esc);
        assert!(app.filter.is_empty());

        // `q` backs out one pane at a time and only quits from the list. ^C
        // quits from wherever the reader is, which is the whole difference
        // between them, and the pane it is pressed in must not swallow it.
        press(&mut app, Key::Enter);
        assert_eq!(app.mode, Mode::Detail);
        assert!(!app.key(Key::Ctrl('c'), H), "^C quits from a detail pane");
        press(&mut app, Key::Esc);

        // The metrics tab: `tab` decides which of its two lists the arrows
        // drive, and each keeps its own index while the other one holds still.
        press(&mut app, Key::Char('3'));
        assert!(!app.on_series);
        press(&mut app, Key::Char('j'));
        assert_eq!(app.nsel, 1, "the names list moves first");
        // Two series under one name, because this corpus has one apiece and a
        // cursor with nowhere to go proves nothing about which one moved.
        app.series = array(
            &crate::api::parse(
                r#"{"series":[{"name":"m","attributes":{"pod":"a"},"points":[["1","1"]]},
                              {"name":"m","attributes":{"pod":"b"},"points":[["1","2"]]}]}"#,
            )
            .unwrap()["series"],
        );
        press(&mut app, Key::Tab);
        assert!(app.on_series);
        press(&mut app, Key::Char('j'));
        assert_eq!((app.nsel, app.ssel), (1, 1), "the series cursor moved");
        press(&mut app, Key::Char('G'));
        assert_eq!(app.ssel, 1, "and clamps at the last series");

        // A record whose trace id is not in this store. Only the store can say
        // so, and it says it on the status bar rather than opening an empty
        // waterfall that reads as a trace with no spans in it.
        press(&mut app, Key::Char('1'));
        app.rows = array(
            &crate::api::parse(
                r#"{"rows":[{"trace_id":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","body":"x"}]}"#,
            )
            .unwrap()["rows"],
        );
        let f = strip(&press(&mut app, Key::Char('t')));
        assert!(app.err, "{f}");
        assert!(f.contains("no spans found for trace cdcd"), "{f}");
        assert_eq!(app.mode, Mode::List, "and the waterfall does not open");
    }

    /// The alert pane's own cursor, and the two answers that are not a list of
    /// rules: a node with no rules file, and Enter pressed on nothing.
    ///
    /// An empty pane must not read as "all clear" — a node evaluating no rules
    /// pages nobody, and that is the one thing an alerting screen cannot let a
    /// reader assume.
    #[test]
    fn the_alert_pane_moves_its_own_cursor_and_says_when_there_are_no_rules() {
        const ROWS: &str = r#"{"rows":[],"stats":{"blocks_total":0,"blocks_scanned":0,
                               "rows_scanned":0,"rows_matched":0}}"#;
        const TWO: &str = r#"{"alerts":[
            {"name":"first","state":"firing","severity":"critical","metric":"count","op":">",
             "threshold":1.0,"value":9.0,"matched":9,"total":null,"over_nano":"60000000000",
             "for_nano":"0","since":"1","firing_since":"1","evaluated_at":"2","signal":"traces",
             "filter":"field:status_code=2","link":"","error":null},
            {"name":"second","state":"ok","severity":"warning","metric":"count","op":">=",
             "threshold":1.0,"value":0.0,"matched":0,"total":null,"over_nano":"300000000000",
             "for_nano":"0","since":null,"firing_since":null,"evaluated_at":"2",
             "signal":"logs","filter":"attr:pod=a","link":"","error":null}],
            "every_nano":"5000000000"}"#;

        // One reply per connection, in the order this session asks for them:
        // the opening list query, `a`, the requery Enter fires, `a` again.
        let addr = source::serve(
            [ROWS, TWO, ROWS, r#"{"alerts":[]}"#]
                .map(source::ok)
                .to_vec(),
        );
        let mut app = App::new(Source::Remote(addr));
        settle(&mut app);

        press(&mut app, Key::Char('a'));
        assert_eq!(app.mode, Mode::Alerts);
        let f = press(&mut app, Key::Char('j'));
        assert_eq!(app.psel, 1, "the pane owns the cursor, not the record list");
        // Read back off the screen: the highlight is on the second rule, which
        // is what Enter is about to act on.
        let row = f
            .lines()
            .find(|l| strip(l).contains("second"))
            .unwrap_or_default();
        assert!(row.contains(term::REV), "{row:?}");

        // Enter follows the *selected* rule: its signal picks the tab and its
        // filter is already spelled in the filter bar's own grammar.
        press(&mut app, Key::Enter);
        assert_eq!(app.tab, Tab::Logs, "the second rule is a logs rule");
        assert_eq!(app.filter, "attr:pod=a");
        assert_eq!(app.mode, Mode::List);

        // A node with no rules file at all.
        let f = strip(&press(&mut app, Key::Char('a')));
        assert_eq!(app.mode, Mode::Alerts, "{f}");
        assert!(f.contains("0 rules") && f.contains("0 firing"), "{f}");
        assert!(f.contains("evaluates no rules"), "{f}");
        // ...where Enter has nothing under the cursor to follow, and must leave
        // the tab and the filter exactly as the reader left them.
        press(&mut app, Key::Enter);
        assert_eq!(app.mode, Mode::Alerts, "enter on no rule opens nothing");
        assert_eq!((app.tab, app.filter.as_str()), (Tab::Logs, "attr:pod=a"));
    }

    /// The name of the test below, as `--exact` wants it.
    const RUN_SELF: &str = "tui::tests::the_event_loop_follows_a_growing_store_and_quits_on_q";

    /// [`run`] itself: `mira mira --data-dir`, on a real terminal, following a
    /// block directory that grows under it.
    ///
    /// Everything else in this file tests the app without the loop, because a
    /// `Term` needs a pty. This is the loop: draw *before* the deferred query so
    /// the "running" frame is on screen while the query blocks, a poll that
    /// times out into a re-query rather than a reload, and `q` returning from
    /// `run` rather than exiting the process — which is what leaves the `Term`
    /// to be dropped and the user's terminal to be handed back.
    ///
    /// Follow mode is proved by data rather than by the indicator: the parent
    /// publishes a block stamped two seconds into the future, so `to: now`
    /// excludes it from the opening query *and* from the reload `f` fires, and
    /// only a tick that ran later can put it on screen. A loop that never timed
    /// out would sit on the older frame until `q`.
    #[test]
    fn the_event_loop_follows_a_growing_store_and_quits_on_q() {
        if let Some(dir) = std::env::var_os("MIRA_TUI_PTY_DIR") {
            // The child. It ends by returning from `run`, not by panicking:
            // the exit path is part of what is under test, and the harness
            // line it then prints to the pty is what the parent reads it off.
            return run(Source::Local(dir.into())).unwrap();
        }
        let dir = store("pty");
        let ahead = crate::api::now_nanos() as u64 + 2_000_000_000;
        let mut b = mira_core::logs::LogsBuilder::new();
        b.append_request(&crate::e2e::logs_export("mira-tail-tick", ahead, 1))
            .unwrap();
        let node = mira_core::block::node_id("b");
        mira_core::block::publish(&dir, "logs", node, 0, 0, &b.finish().unwrap()).unwrap();

        let crate::term::tests::Pty {
            screen,
            err,
            stalled,
            trailing,
        } = crate::term::tests::drive(
            RUN_SELF,
            &[("MIRA_TUI_PTY_DIR", dir.as_os_str())],
            &[
                // The opening screen — drawn, queried, drawn again — then `f`.
                ("checkout handled request 4", b"f"),
                // The reload it fires, which still cannot see the future block.
                ("● follow", b""),
                // ...and the tick three seconds later, which can. `q` from the
                // list is the quit.
                ("mira-tail-tick handled request 0", b"q"),
            ],
            "\x1b[?25h\x1b[?1049l",
        );
        assert!(
            stalled.is_none(),
            "child stalled at {stalled:?}: {err}\nscreen:\n{screen:?}"
        );
        // `q` returned from `run`; it did not `exit`, panic or die of a signal.
        // The harness line is printed after the alternate screen was already
        // handed back, which is why it is on the pty at all.
        assert!(err.is_empty(), "{err}");
        assert!(screen.contains("1 passed"), "{screen:?}");
        assert!(trailing, "terminal not restored: {screen:?}");
        // The screen the reader would have been looking at: the tick's row is
        // above the rows that were already there, not instead of them.
        let tick = screen.find("mira-tail-tick").unwrap();
        let old = screen.rfind("checkout handled request 4").unwrap();
        assert!(tick < old, "the tick replaced the window: {screen:?}");
    }
}
