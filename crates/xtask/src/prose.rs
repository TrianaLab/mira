//! How long a page may go without giving the reader a handhold.
//!
//! markdownlint checks the markup and Vale checks the words. Neither can see
//! the shape of a page, which is the thing this repository actually got wrong:
//! `docs/architecture.md` once ran eight and a half thousand words under a
//! single `## 11. Performance model` with nothing to navigate by, and every
//! reader said the same thing about it.
//!
//! Four limits, all structural, all mechanical to satisfy:
//!
//! | Limit | Why that one |
//! | --- | --- |
//! | [`MAX_PAGE_WORDS`] | a page is one idea, and one idea fits |
//! | [`MAX_SECTION_WORDS`] | the distance between two headings is the distance a reader has to hold everything in their head |
//! | [`MAX_HEADING_DEPTH`] | four levels is a section, five is an outline nobody follows |
//! | [`MAX_SENTENCE_WORDS`] | the same problem one level down |
//!
//! Only prose counts. Fences, tables, footnote definitions and generated blocks
//! are skipped, because a diagram is not something a reader wades through, a
//! citation is followed rather than read, and a generated row is not this page's
//! writing — the budget is on what was written, so that the way to get under it
//! is to write less rather than to move text into a table.
//!
//! The last one was a Vale rule first, and Vale is the better place for anything
//! about words — except that its sentence segmenter reads `6.7x.` as an
//! abbreviation and runs two sentences together, which on a page that quotes a
//! ratio every other line is most of them. Here the segmentation is [a function
//! with tests](fn@starts_sentence) rather than a tokeniser nobody in this
//! repository can fix.
//!
//! ```console
//! $ cargo run -p xtask -- prose docs/architecture.md README.md
//! ```
//!
//! The file list comes from the caller — the Makefile's `PROSE` — rather than
//! from a glob here, because markdownlint and Vale need the same list and three
//! definitions of "the pages this repository wrote" is two too many.

use crate::util::{Failures, read};

/// Words on one page, counting prose only.
///
/// Five minutes of reading. A page over it is not a long page, it is two pages
/// that were never separated, and the reader pays for the decision either way.
const MAX_PAGE_WORDS: usize = 1200;

/// Words between two headings, counting prose only.
///
/// 250 words is a minute. The point of the number is not the minute: it is that
/// an idea explained in 250 words was understood by whoever wrote it, and one
/// that needs 900 was being worked out on the page. Subheadings are a legal fix
/// and a bad one — a wall split into four is still a wall.
const MAX_SECTION_WORDS: usize = 250;

/// `####` is the last level with a name a reader can hold. `#####` is the level
/// at which the page wanted to be two pages.
const MAX_HEADING_DEPTH: usize = 4;

/// Words in one sentence.
///
/// The corpus medians 24 and its 95th percentile is 53, so this is the tail and
/// not the middle: every long sentence already written here stays. What it
/// catches is the paragraph that forgot to end.
const MAX_SENTENCE_WORDS: usize = 60;

/// Check every path given. Returns whether they all passed.
pub fn check(paths: &[&str]) -> bool {
    let mut f = Failures::default();
    for path in paths {
        match read(path) {
            Ok(text) => check_one(path, &text, &mut f),
            Err(e) => f.fail(e),
        }
    }
    if f.is_empty() {
        println!("prose: {} pages within the structural limits.", paths.len());
    }
    f.report("pages over a structural limit")
}

/// One heading and everything under it, until the next heading of any level.
struct Section {
    heading: String,
    line: usize,
    words: usize,
}

/// A run of words with no full stop in it, and the line it started on.
#[derive(Default)]
struct Sentence {
    line: usize,
    words: usize,
}

fn check_one(path: &str, text: &str, f: &mut Failures) {
    let mut section = Section {
        // Everything before the first heading still has to be bounded: a page
        // that opens with a thousand words and then starts its headings is the
        // same wall with the title on top of it.
        heading: "the text above the first heading".into(),
        line: 1,
        words: 0,
    };
    let mut sentence = Sentence::default();
    let mut page = 0usize;
    // Whether the token before this one ended in a full stop. A sentence ends
    // where a stop is *followed by* a capital, so the decision is always one
    // token late, and the flag is what carries it across a line break.
    let mut stopped = false;
    // Three kinds of line are not prose and none of them tires a reader the way
    // a paragraph does: a code block is skimmed or run, a table is looked up
    // rather than read, and a generated block is not this page's writing at all
    // — counting it would make `xtask measurements render` able to fail this
    // gate by adding a row.
    let mut in_fence = false;
    let mut in_comment = false;
    let mut in_generated = false;

    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        // A sentence ends at the first thing that is not more of the same
        // paragraph, stop or no stop — otherwise a bullet list nobody
        // punctuated reads as one sentence with forty words in it.
        if !continues_a_sentence(trimmed) {
            report_sentence(path, &sentence, f);
            sentence = Sentence::default();
            stopped = false;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.contains("BEGIN GENERATED") {
            in_generated = true;
        } else if trimmed.contains("END GENERATED") {
            in_generated = false;
            continue;
        }
        if trimmed.starts_with("<!--") {
            // A one-line comment opens and closes on the same line; a block one
            // stays open until the reader finds `-->`.
            in_comment = !trimmed.contains("-->");
            continue;
        }
        if in_comment {
            in_comment = !trimmed.contains("-->");
            continue;
        }
        // A footnote definition is a citation, not a paragraph: nobody reads
        // `docs/market.md`'s fifty-six of them top to bottom, they follow one
        // marker and come back. Same argument as the table above it, and the
        // alternative is a page that has to choose between sourcing a
        // competitor's number and staying inside the budget.
        if in_generated || trimmed.starts_with('|') || trimmed.starts_with("[^") {
            continue;
        }

        let Some(depth) = heading_depth(trimmed) else {
            for tok in trimmed.split_whitespace() {
                if stopped && starts_sentence(tok) {
                    report_sentence(path, &sentence, f);
                    sentence = Sentence::default();
                }
                if sentence.words == 0 {
                    sentence.line = i + 1;
                }
                sentence.words += 1;
                section.words += 1;
                page += 1;
                stopped = ends_sentence(tok);
            }
            continue;
        };
        report(path, &section, f);
        if depth > MAX_HEADING_DEPTH {
            f.fail(format!(
                "{path}:{} is an h{depth}: {trimmed:?}\n    \
                 the limit is h{MAX_HEADING_DEPTH}. A page that needs a fifth level \
                 is a page that wants to be two.",
                i + 1
            ));
        }
        section = Section {
            heading: trimmed.trim_start_matches('#').trim().to_string(),
            line: i + 1,
            words: 0,
        };
    }
    report(path, &section, f);
    report_sentence(path, &sentence, f);
    if page > MAX_PAGE_WORDS {
        f.fail(format!(
            "{path} is {page} words of prose\n    \
             the limit is {MAX_PAGE_WORDS}. Cut it, or split the page and link.",
        ));
    }
}

/// Whether this line is more of the paragraph above it.
///
/// `[^m1]:` is here because a run of footnote definitions has no blank line
/// between one and the next, and reading fifteen of them as one sentence was
/// this gate's first and loudest false positive.
fn continues_a_sentence(trimmed: &str) -> bool {
    let list_marker = matches!(trimmed.split_once(' '), Some((m, _))
        if m == "-" || m == "*" || m == "+" || m.trim_end_matches('.').parse::<u32>().is_ok());
    !(trimmed.is_empty()
        || list_marker
        || trimmed.starts_with("```")
        || trimmed.starts_with("~~~")
        || trimmed.starts_with('|')
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("[^")
        || heading_depth(trimmed).is_some())
}

/// Whether a full stop here is the end of a sentence or part of a word.
///
/// The answer is what follows it. `6.7x. An` is two sentences and `e.g. the` is
/// one, and the difference is the capital — which is also why this is deliberately
/// wrong about `Dr. Smith`, a form that does not appear in these pages and would
/// cost an abbreviation list to get right.
///
/// A code span counts as a capital. Half the sentences on these pages open with
/// one — `wal.lock_wait` is 82% of submit time — and without this the gate
/// reports them as continuations of the paragraph above and asks for a rewrite
/// that would not improve anything. The form it gets wrong in exchange,
/// ``e.g. `foo` ``, appears nowhere in the corpus.
fn starts_sentence(tok: &str) -> bool {
    tok.trim_start_matches(['(', '[', '"', '\'', '*', '_'])
        .chars()
        .next()
        .is_some_and(|c| c.is_uppercase() || c == '`')
}

fn ends_sentence(tok: &str) -> bool {
    tok.trim_end_matches([')', ']', '"', '\'', '*', '_', '`'])
        .ends_with(['.', '!', '?'])
}

/// `###` → `Some(3)`. Not a heading → `None`.
///
/// The space is required: `#!/bin/sh` inside an indented block is not an h1,
/// and `#` alone is not a heading either.
fn heading_depth(trimmed: &str) -> Option<usize> {
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    (1..=6).contains(&hashes).then_some(hashes).filter(|_| {
        trimmed[hashes..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    })
}

fn report_sentence(path: &str, s: &Sentence, f: &mut Failures) {
    if s.words > MAX_SENTENCE_WORDS {
        f.fail(format!(
            "{path}:{} starts a sentence {} words long\n    \
             the limit is {MAX_SENTENCE_WORDS}. Split it, or cut it.",
            s.line, s.words
        ));
    }
}

fn report(path: &str, s: &Section, f: &mut Failures) {
    if s.words > MAX_SECTION_WORDS {
        f.fail(format!(
            "{path}:{} runs {} words before the next heading: {:?}\n    \
             the limit is {MAX_SECTION_WORDS}. Split it with a subheading, or cut it.",
            s.line, s.words, s.heading
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(text: &str) -> Vec<String> {
        let mut f = Failures::default();
        check_one("p.md", text, &mut f);
        f.0
    }

    /// `n` words of ordinary punctuated prose, for the tests that are about
    /// sections rather than sentences.
    fn words(n: usize) -> String {
        (0..n)
            .map(|i| match i % 10 {
                0 => "Word",
                9 => "word.",
                _ => "word",
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `n` words with no full stop anywhere in them: one sentence.
    fn run(n: usize) -> String {
        vec!["word"; n].join(" ")
    }

    #[test]
    fn a_section_under_the_limit_passes() {
        assert!(findings(&format!("# Title\n\n{}\n", words(MAX_SECTION_WORDS))).is_empty());
    }

    #[test]
    fn a_section_over_the_limit_names_its_heading_and_its_count() {
        let f = findings(&format!("# Title\n\n{}\n", words(MAX_SECTION_WORDS + 1)));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("p.md:1"), "{f:?}");
        assert!(f[0].contains("251 words"), "{f:?}");
        assert!(f[0].contains("Title"), "{f:?}");
    }

    #[test]
    fn the_text_above_the_first_heading_is_bounded_too() {
        let f = findings(&format!("{}\n\n# Title\n", words(MAX_SECTION_WORDS + 1)));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("above the first heading"), "{f:?}");
    }

    #[test]
    fn a_page_over_the_limit_is_a_finding_and_a_subheading_does_not_fix_it() {
        let body = format!("## S\n\n{}\n", words(MAX_SECTION_WORDS));
        let page = format!(
            "# T\n\n{}",
            body.repeat(MAX_PAGE_WORDS / MAX_SECTION_WORDS + 1)
        );
        let f = findings(&page);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("1250 words of prose"), "{f:?}");
    }

    #[test]
    fn a_subheading_resets_the_count() {
        let half = words(MAX_SECTION_WORDS);
        assert!(findings(&format!("# Title\n\n{half}\n\n## Next\n\n{half}\n")).is_empty());
    }

    /// The three things a page can be long *with* that do not read like prose.
    /// Without these the gate would fire on the registry table in the
    /// measurement contract, which no human wrote.
    #[test]
    fn code_tables_and_generated_blocks_are_not_prose() {
        let long = words(MAX_SECTION_WORDS + 1);
        for body in [
            format!("```text\n{long}\n```"),
            format!("| a | b |\n| --- | --- |\n| {long} | x |"),
            format!("<!-- BEGIN GENERATED: t -->\n{long}\n<!-- END GENERATED: t -->"),
            format!("<!--\n{long}\n-->"),
            format!("[^a]: {long}"),
        ] {
            let f = findings(&format!("# Title\n\n{body}\n"));
            assert!(f.is_empty(), "{body:.40}: {f:?}");
        }
    }

    /// A fence that opens and never closes used to swallow the rest of the
    /// page, which turned the limit off for everything below it.
    #[test]
    fn an_unclosed_fence_does_not_disable_the_limit_for_the_rest_of_the_page() {
        let f = findings(&format!(
            "# Title\n\n```text\nx\n```\n\n{}\n",
            words(MAX_SECTION_WORDS + 1)
        ));
        assert_eq!(f.len(), 1, "{f:?}");
    }

    #[test]
    fn a_fifth_heading_level_is_a_finding_and_a_fourth_is_not() {
        assert!(findings("# a\n\n#### d\n").is_empty());
        let f = findings("# a\n\n##### e\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("is an h5"), "{f:?}");
    }

    /// `#` needs a space after it. Without this, a shell comment in an indented
    /// block — which is not a fence, so nothing else skips it — resets the
    /// section count and the limit silently stops applying.
    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        assert_eq!(heading_depth("#no space"), None);
        assert_eq!(heading_depth("#!/bin/sh"), None);
        assert_eq!(heading_depth("####### seven"), None);
        assert_eq!(heading_depth("## two"), Some(2));
    }

    #[test]
    fn a_sentence_over_the_limit_names_the_line_it_started_on() {
        let long = format!("# T\n\nShort one. Alpha {}.\n", run(MAX_SENTENCE_WORDS));
        let f = findings(&long);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("p.md:3"), "{f:?}");
        assert!(f[0].contains("61 words"), "{f:?}");
        assert!(findings(&format!("# T\n\n{}.\n", run(MAX_SENTENCE_WORDS))).is_empty());
    }

    /// The reason this is not the Vale rule it replaces: Vale's segmenter reads
    /// `6.7x.` as an abbreviation, runs the two sentences together, and reports
    /// a 60-word violation on two 40-word sentences.
    #[test]
    fn a_stop_after_a_ratio_ends_the_sentence() {
        let half = run(MAX_SENTENCE_WORDS - 20);
        assert!(findings(&format!("# T\n\nAlpha {half} 6.7x. Bravo {half}.\n")).is_empty());
    }

    /// The other half of the same decision: a stop followed by a lower-case word
    /// is inside one, so a limit set from the corpus is not quietly halved.
    #[test]
    fn an_abbreviation_does_not_end_one() {
        let half = run(MAX_SENTENCE_WORDS / 2);
        let f = findings(&format!("# T\n\nAlpha {half} e.g. beta {half}.\n"));
        assert_eq!(f.len(), 1, "{f:?}");
    }

    /// Two forms that are not prose continuations, and both of them ran whole
    /// blocks together into one sentence before they were handled.
    #[test]
    fn a_code_span_and_a_footnote_both_start_something_new() {
        let half = run(MAX_SENTENCE_WORDS - 20);
        assert!(findings(&format!("# T\n\nAlpha {half} 6.7x. `foo` {half}.\n")).is_empty());
        let note = format!("[^a]: Alpha {half}.\n[^b]: Bravo {half}.\n");
        assert!(findings(&format!("# T\n\n{note}")).is_empty());
    }

    /// A list nobody punctuated is not one long sentence, and a sentence that
    /// wraps across two lines is not two short ones.
    #[test]
    fn a_bullet_ends_a_sentence_and_a_line_break_does_not() {
        let third = run(MAX_SENTENCE_WORDS / 3);
        assert!(findings(&format!("# T\n\n- {third}\n- {third}\n- {third}\n")).is_empty());
        let f = findings(&format!("# T\n\n{third}\n{third}\n{third} and one more.\n"));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("p.md:3"), "{f:?}");
    }

    #[test]
    fn a_path_that_does_not_exist_is_a_finding_not_a_pass() {
        assert!(!check(&["docs/there-is-no-such-page.md"]));
    }
}
