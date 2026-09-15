//! The claim tally on the market page, counted from the tables it is about.
//!
//! `docs/market.md` compares Mira against published figures, and every
//! comparable row carries an `=` cell saying whether the two numbers can
//! honestly sit side by side: `—` for Mira's own rows, `no`, `yes` or `~` for
//! everyone else's. The paragraph above those tables totals them up.
//!
//! That paragraph was inside `<!-- BEGIN GENERATED: market-claim-tally -->`
//! markers with no generator behind them — hand-counted, labelled as though it
//! were not, and wrong the first time a row moved. This is the generator.
//!
//! ```console
//! $ cargo run -p xtask -- market          # the tally matches the tables
//! $ cargo run -p xtask -- market render   # rewrite it from them
//! ```

use crate::reference::inject;
use crate::util::{Failures, read_or_exit};

const PAGE: &str = "docs/market.md";
const MARKER: &str = "market-claim-tally";

/// Fail if the tally in the page is not what its tables add up to.
pub fn check() -> bool {
    let mut f = Failures::default();
    let text = read_or_exit(PAGE);
    let want = match tally(&text).map(|t| sentence(&t)) {
        Ok(s) => s,
        Err(e) => {
            f.fail(e);
            return f.report("market tally");
        }
    };
    match block(&text) {
        Some(have) if have.trim() == want.trim() => {
            println!("market: the claim tally matches the tables.");
            true
        }
        Some(have) => {
            f.fail(format!(
                "{PAGE}'s claim tally is stale.\n    it says:  {}\n    tables:   {}\n    \
                 run `make market` to rewrite it.",
                have.trim().replace('\n', " "),
                want.trim().replace('\n', " ")
            ));
            f.report("market tally")
        }
        None => {
            f.fail(format!(
                "{PAGE} has no `<!-- BEGIN GENERATED: {MARKER} -->` block."
            ));
            f.report("market tally")
        }
    }
}

/// Rewrite the tally from the tables.
pub fn render() -> bool {
    let mut f = Failures::default();
    let text = read_or_exit(PAGE);
    match tally(&text) {
        Ok(t) => {
            inject(PAGE, MARKER, &sentence(&t), &mut f);
        }
        Err(e) => f.fail(e),
    }
    f.report("market render failed")
}

/// What the `=` column says, across every table that has one.
#[derive(Debug, Default, PartialEq)]
struct Tally {
    tables: usize,
    /// `—`: Mira's own row, which has nothing to be comparable *to*.
    mira: usize,
    no: usize,
    yes: usize,
    /// `~`: comparable with a caveat spelled out in the row's reason.
    partial: usize,
}

fn tally(text: &str) -> Result<Tally, String> {
    let mut t = Tally::default();
    // `None` between tables. A table's `=` column can be any index — it is the
    // fourth in some and the fifth in others — so the header is what says where
    // to look, and a table without one is a table of something else.
    let mut column: Option<usize> = None;
    for line in text.lines() {
        if !line.starts_with('|') {
            column = None;
            continue;
        }
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        let Some(at) = column else {
            if let Some(at) = cells.iter().position(|c| *c == "=") {
                column = Some(at);
                t.tables += 1;
            }
            continue;
        };
        // The delimiter row, `| --- | --- |`.
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        match cells.get(at).copied() {
            Some("—") => t.mira += 1,
            Some("no") => t.no += 1,
            Some("yes") => t.yes += 1,
            Some("~") => t.partial += 1,
            // A blank or a footnote marker is a row that is not making a claim.
            _ => {}
        }
    }
    if t.tables == 0 {
        return Err(format!(
            "{PAGE} has no table with an `=` column. Either the pages moved or the \
             column was renamed — and a tally counted from nothing would read as zero \
             disputed claims, which is the most flattering possible lie."
        ));
    }
    Ok(t)
}

fn sentence(t: &Tally) -> String {
    let marked = t.mira + t.no + t.yes + t.partial;
    let theirs = t.no + t.yes + t.partial;
    let mut rest = Vec::new();
    if t.yes > 0 {
        rest.push(format!("{} are `yes`", count(t.yes)));
    }
    if t.partial > 0 {
        rest.push(format!(
            "{} {} `~`",
            count(t.partial),
            if t.partial == 1 { "is" } else { "are" }
        ));
    }
    wrap(&format!(
        "Across the {} tables that carry one there are {marked} marked rows. {} are \
         Mira's own and take `—`; of the {theirs} competitor claims, **{} are `no`**{}{}.",
        count(t.tables),
        count(t.mira),
        count(t.no),
        if rest.is_empty() { "" } else { ", " },
        rest.join(" and "),
    ))
}

/// Spelled out below ten, digits from ten up — the convention the surrounding
/// prose already follows, so the generated sentence does not read as generated.
fn count(n: usize) -> String {
    [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ]
    .get(n)
    .map_or_else(|| n.to_string(), ToString::to_string)
}

/// Greedy wrap at 80 columns.
///
/// The rest of the page is wrapped, and a generated paragraph that is one long
/// line makes every future re-render a one-line diff of the whole paragraph.
/// Columns are characters, not bytes: the sentence contains an em dash, and
/// counting its three bytes would wrap this paragraph two characters early.
fn wrap(s: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for word in s.split_whitespace() {
        let width = word.chars().count();
        if col > 0 && col + 1 + width > 80 {
            out.push('\n');
            col = 0;
        } else if col > 0 {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += width;
    }
    out
}

fn block(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once(&format!("<!-- BEGIN GENERATED: {MARKER} -->"))?;
    let (body, _) = rest.split_once(&format!("<!-- END GENERATED: {MARKER} -->"))?;
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_WITH_TWO_TABLES: &str = "\
| Engine | Published | = | Reason |
| --- | --- | --- | --- |
| **Mira** | 1 | — | — |
| Other | 2 | no | wrong axis |
| Third | 3 | yes | same axis |

prose in between, which ends the table

| Vendor | Basis | = |
| --- | --- | --- |
| **Mira** | x | — |
| Other | y | ~ |
";

    #[test]
    fn the_tally_counts_every_table_that_has_an_equals_column() {
        assert_eq!(
            tally(PAGE_WITH_TWO_TABLES).unwrap(),
            Tally {
                tables: 2,
                mira: 2,
                no: 1,
                yes: 1,
                partial: 1
            }
        );
    }

    /// The `=` column is the third in one of those tables and the second in the
    /// other. Reading a fixed index would have counted the reason text.
    #[test]
    fn the_column_is_found_per_table_rather_than_assumed() {
        let one_column_over =
            PAGE_WITH_TWO_TABLES.replace("| Vendor | Basis | = |", "| Vendor | = | Basis |");
        assert_eq!(
            tally(&one_column_over).unwrap().partial,
            0,
            "moved without moving its cell"
        );
    }

    #[test]
    fn a_page_with_no_equals_column_is_an_error_rather_than_a_tally_of_zero() {
        assert!(tally("| a | b |\n| --- | --- |\n| 1 | 2 |\n").is_err());
    }

    #[test]
    fn the_sentence_reads_as_prose_at_one_and_at_many() {
        let one = sentence(&Tally {
            tables: 1,
            mira: 1,
            no: 1,
            yes: 0,
            partial: 1,
        });
        assert!(one.contains("one is `~`"), "{one}");
        assert!(!one.contains("are `yes`"), "{one}");
        // Unwrapped: where the line breaks fall is `wrap`'s business, tested
        // below, and asserting on it here would make this test about columns.
        let many = sentence(&Tally {
            tables: 6,
            mira: 13,
            no: 38,
            yes: 10,
            partial: 2,
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        assert!(many.contains("Across the six tables"), "{many}");
        assert!(
            many.contains("**38 are `no`**, 10 are `yes` and two are `~`"),
            "{many}"
        );
        assert!(many.contains("of the 50 competitor claims"), "{many}");
    }

    #[test]
    fn the_wrap_keeps_every_line_inside_eighty_columns() {
        let s = sentence(&Tally {
            tables: 6,
            mira: 13,
            no: 38,
            yes: 10,
            partial: 1,
        });
        assert!(s.contains('\n'), "{s}");
        assert!(s.lines().all(|l| l.chars().count() <= 80), "{s}");
    }

    /// The real page, which is what CI runs. A tally that disagrees with its own
    /// tables is the whole reason this module exists.
    #[test]
    fn the_committed_page_tallies() {
        assert!(check());
    }
}
