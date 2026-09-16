//! Every screen row in the TUI, drawn by ratatui.
//!
//! The seam is `Term::draw`, which takes one ANSI string per
//! screen row. `Term` keeps the pty, the raw mode, the resize handling and the
//! key decoding; only content generation lives here. A widget renders into a
//! [`Buffer`] and [`flatten`] turns that buffer back into the rows `draw`
//! already paints, so the port cost nothing at the boundary — `term::Row`, the
//! hand-rolled width-tracking line builder this replaces, is gone rather than
//! wrapped.
//!
//! **No backend is linked.** ratatui's `CrosstermBackend` would bring crossterm,
//! mio, signal-hook and parking_lot to do what `term.rs` already does, so the
//! dependency is `ratatui` with `default-features = false`: the widgets and the
//! layout solver, nothing that touches a terminal. That is most of why the
//! measured cost is +26 crates rather than the +48 a default build wanted.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{
    Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Table,
    Widget,
};
use yaml_rust2::Yaml;

use super::{clip, dur, hms, i64_of, kind, pairs, service, stamp};
use crate::term;

/// Render into a fresh `w × h` buffer and flatten it to rows.
fn paint(w: usize, h: usize, f: impl FnOnce(Rect, &mut Buffer)) -> Vec<String> {
    let area = Rect::new(0, 0, w as u16, h as u16);
    let mut buf = Buffer::empty(area);
    f(area, &mut buf);
    flatten(&buf, area)
}

/// One ANSI string per buffer line, coalescing runs that share a style.
///
/// Per cell would be correct and unreadable on the wire: a 200-column row would
/// carry 200 escapes for a line that changes colour twice. `Term::draw` writes
/// whatever it is handed, so the coalescing has to happen here.
fn flatten(buf: &Buffer, area: Rect) -> Vec<String> {
    let mut out = Vec::with_capacity(area.height as usize);
    for y in area.top()..area.bottom() {
        let mut line = String::new();
        let mut open: Option<(Color, Modifier)> = None;
        for x in area.left()..area.right() {
            let Some(c) = buf.cell((x, y)) else { continue };
            let key = (c.fg, c.modifier);
            if open != Some(key) {
                line.push_str(term::RESET);
                line.push_str(&sgr(c.fg, c.modifier));
                open = Some(key);
            }
            line.push_str(c.symbol());
        }
        line.push_str(term::RESET);
        out.push(line);
    }
    out
}

/// The SGR run for one cell's style, in the escapes `term` already declares.
///
/// Only the palette Mira uses is mapped, and background colour is not part of
/// it — the one thing drawn on a coloured field is the selection, and that is
/// reverse video so it inherits whatever the terminal's own scheme is. A
/// migration that invented a 256-colour theme on the way past would be changing
/// the program rather than porting it.
fn sgr(fg: Color, m: Modifier) -> String {
    let mut s = String::new();
    if m.contains(Modifier::BOLD) {
        s.push_str(term::BOLD);
    }
    if m.contains(Modifier::DIM) {
        s.push_str(term::DIM);
    }
    if m.contains(Modifier::REVERSED) {
        s.push_str(term::REV);
    }
    s.push_str(match fg {
        Color::Red => term::RED,
        Color::Green => term::GREEN,
        Color::Yellow => term::YELLOW,
        Color::Blue => term::BLUE,
        Color::Magenta => term::MAGENTA,
        Color::Cyan => term::CYAN,
        _ => "",
    });
    s
}

/// The style a severity is drawn in: the OTLP number bands, not a guess at
/// `severity_text`, which is free-form and frequently absent.
pub fn sev(n: i64) -> Style {
    match n {
        17.. => Style::default().fg(Color::Red),
        13..=16 => Style::default().fg(Color::Yellow),
        9..=12 => Style::default().fg(Color::Green),
        _ => Style::default().add_modifier(Modifier::DIM),
    }
}

/// One screen row from spans, clipped and padded to `w`.
///
/// The replacement for `term::Row`. Width there was tracked by hand, separately
/// from the byte length, because inline ANSI makes `len()` a lie — that was the
/// entire reason the type existed. Here the escapes are applied on the way out
/// of the buffer, so every truncation and pad is over plain graphemes and
/// belongs to the library.
pub fn row(w: usize, spans: Vec<Span<'_>>) -> String {
    row_styled(w, Style::default(), spans)
}

/// [`row`], with a style under the whole width rather than under the text.
///
/// This is what `Row::fill` was for: a reverse-video row that stops at its last
/// character reads as a ragged highlight, so the selection has to cover the
/// padding too. Cell styles merge rather than replace, so a span that sets a
/// colour keeps it and inherits the reverse.
pub fn row_styled(w: usize, base: Style, spans: Vec<Span<'_>>) -> String {
    paint(w, 1, |area, buf| {
        Paragraph::new(TLine::from(spans))
            .style(base)
            .render(area, buf);
    })
    .pop()
    .unwrap_or_default()
}

/// The style a selected row is drawn in.
pub fn hl(on: bool) -> Style {
    match on {
        true => Style::default().add_modifier(Modifier::REVERSED),
        false => Style::default(),
    }
}

/// One row laid out in fixed columns, each clipped to its own column.
///
/// The hand-rolled version did this with a `cap`/`pad_to` pair around every
/// write, and the reason was worth keeping: an over-long span name has to eat
/// into its own column rather than push the duration off the right-hand edge.
/// A `Layout` states the same thing once, in the units the columns are in.
pub fn columns(w: usize, base: Style, cells: Vec<(u16, Vec<Span<'_>>)>) -> String {
    paint(w, 1, |area, buf| {
        buf.set_style(area, base);
        let widths: Vec<Constraint> = cells.iter().map(|(n, _)| Constraint::Length(*n)).collect();
        let areas = Layout::horizontal(widths).split(area);
        for (a, (_, spans)) in areas.iter().zip(cells) {
            Paragraph::new(TLine::from(spans)).render(*a, buf);
        }
    })
    .pop()
    .unwrap_or_default()
}

/// A bar with one group pinned left and one pinned right.
///
/// A `Layout` reserves the right group's width before the left group is drawn,
/// so a narrow terminal clips the left. For every bar here that is the right
/// way round: the window and the limit say what the filter was applied to, and
/// losing them to a long filter string is what the hand-rolled `pad_to` did.
pub fn bar(w: usize, left: Vec<Span<'_>>, right: Vec<Span<'_>>) -> String {
    let rw = right.iter().map(Span::width).sum::<usize>() as u16;
    paint(w, 1, |area, buf| {
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
        Paragraph::new(TLine::from(left)).render(l, buf);
        Paragraph::new(TLine::from(right)).render(r, buf);
    })
    .pop()
    .unwrap_or_default()
}

/// A full-width rule with a title set into it.
pub fn rule(w: usize, title: &str) -> String {
    let used = 2 + title.chars().count();
    row(
        w,
        vec![
            "──".dim(),
            title.to_string().dim(),
            "─".repeat(w.saturating_sub(used)).dim(),
        ],
    )
}

/// The log list: one `Table` of four columns, with a scrollbar in the gutter.
///
/// The gutter is what the hand-rolled list does not have. `window_start` tells
/// the reader which rows are on screen and nothing tells them where that window
/// sits in the result — a scrollbar is the answer, and it is four lines here
/// because `Scrollbar` is a widget rather than something to draw.
pub fn log_list(rows: &[Yaml], w: usize, h: usize, sel: usize, start: usize) -> Vec<String> {
    let visible: Vec<Row> = rows
        .iter()
        .skip(start)
        .take(h)
        .enumerate()
        .map(|(i, row)| {
            // Under the selection the per-cell styles are dropped rather than
            // merged: reverse video over a dim service name renders as a faded
            // patch in the middle of the highlight.
            let picked = start + i == sel;
            let dim = match picked {
                true => Style::default(),
                false => Style::default().add_modifier(Modifier::DIM),
            };
            let n = row["severity_number"].as_i64().unwrap_or(0);
            Row::new(vec![
                Cell::from(format!(" {}", hms(i64_of(&row["time_unix_nano"])))),
                Cell::from(clip(row["severity_text"].as_str().unwrap_or("-"), 6))
                    .style(if picked { Style::default() } else { sev(n) }),
                Cell::from(clip(service(row), 16)).style(dim),
                Cell::from(row["body"].as_str().unwrap_or("").to_owned()),
            ])
            .style(hl(picked))
        })
        .collect();

    paint(w, h, |area, buf| {
        // One column short, so the scrollbar has a gutter to live in.
        let body = Rect {
            width: area.width.saturating_sub(1),
            ..area
        };
        let table = Table::new(
            visible,
            [
                Constraint::Length(13),
                Constraint::Length(6),
                Constraint::Length(16),
                Constraint::Min(0),
            ],
        )
        .column_spacing(1);
        Widget::render(table, body, buf);

        let mut state = ScrollbarState::new(rows.len()).position(sel);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .render(area, buf, &mut state);
    })
}

/// One record's fields, its attributes, then its events and links — each
/// section a two-column `Table` under a rule.
///
/// The key column is measured from the data rather than fixed, and measured
/// across every section so the values line up down the whole pane. That is the
/// bug this fixes: the hand-rolled version wrote `{k:<26}`, which pads a short
/// key and does nothing at all to a long one, so `deployment.environment.name`
/// — 27 characters — rendered as `deployment.environment.nameprod`.
pub fn detail(row: &Yaml, w: usize) -> Vec<String> {
    let mut fields: Vec<(String, String, Color)> = Vec::new();
    for (k, v) in pairs(row) {
        if matches!(k.as_str(), "attributes" | "events" | "links" | "points") {
            continue;
        }
        let pretty = match k.as_str() {
            "time_unix_nano" | "observed_time_unix_nano" | "start_time_unix_nano" => {
                v.parse::<i64>().map(stamp).unwrap_or_else(|_| v.clone())
            }
            "duration_nano" => v.parse::<i64>().map(dur).unwrap_or_else(|_| v.clone()),
            "kind" => v
                .parse::<i64>()
                .map(|k| kind(k).to_owned())
                .unwrap_or_else(|_| v.clone()),
            _ => v.clone(),
        };
        fields.push((k, pretty, Color::Reset));
    }

    let mut groups = vec![("", fields)];
    let attrs = pairs(&row["attributes"]);
    if !attrs.is_empty() {
        groups.push((
            " attributes ",
            attrs
                .into_iter()
                .map(|(k, v)| (k, v, Color::Cyan))
                .collect(),
        ));
    }
    // An event's own attributes are the payload of the event — an exception's
    // type and stacktrace live nowhere else — and this is the only screen that
    // ever shows them, so they are indented under it rather than dropped.
    for (label, key) in [(" events ", "events"), (" links ", "links")] {
        let items = row[key].as_vec().map_or(&[][..], |v| v.as_slice());
        if items.is_empty() {
            continue;
        }
        let mut rows = Vec::new();
        for it in items {
            for (k, v) in pairs(it) {
                if k == "attributes" {
                    continue;
                }
                rows.push((k, v, Color::Reset));
            }
            for (k, v) in pairs(&it["attributes"]) {
                rows.push((format!("  {k}"), v, Color::Cyan));
            }
        }
        groups.push((label, rows));
    }

    // The widest key anywhere in the pane, capped at a third of the screen so
    // one pathological attribute name cannot squeeze every value off the edge.
    let keyw = groups
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(k, _, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        .min(w / 3) as u16;

    let mut out = Vec::new();
    for (label, rows) in groups {
        if !label.is_empty() {
            out.push(rule(w, label));
        }
        if rows.is_empty() {
            continue;
        }
        let table: Vec<Row> = rows
            .iter()
            .map(|(k, v, c)| {
                Row::new(vec![
                    Cell::from(k.clone()).style(Style::default().add_modifier(Modifier::DIM)),
                    Cell::from(v.clone()).style(Style::default().fg(*c)),
                ])
            })
            .collect();
        out.extend(paint(w, rows.len(), |area, buf| {
            // Two columns of gutter, as the hand-rolled pane had — a key hard
            // against the left edge reads as part of the rule above it.
            let body = Rect {
                x: 2,
                width: area.width.saturating_sub(2),
                ..area
            };
            let t =
                Table::new(table, [Constraint::Length(keyw), Constraint::Min(0)]).column_spacing(2);
            Widget::render(t, body, buf);
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tests::strip;

    /// The bug this pane is being ported to fix: a key at or past the old fixed
    /// 26-column pad ran straight into its value. Measuring the column from the
    /// data keeps a gap whatever the key is.
    #[test]
    fn a_long_attribute_key_does_not_abut_its_value() {
        let y = yaml_rust2::YamlLoader::load_from_str(
            "body: hi\nattributes:\n  deployment.environment.name: prod\n",
        )
        .unwrap()
        .remove(0);
        let out = detail(&y, 100);
        let line = out
            .iter()
            .map(|l| strip(l))
            .find(|l| l.contains("prod"))
            .expect("the attribute is rendered");
        assert!(
            line.contains("name  prod") || line.contains("name prod"),
            "key and value must not run together: {line:?}"
        );
    }
}
