//! Cross-references in the Markdown mkdocs does not build.
//!
//! `mkdocs build --strict` resolves every link and every anchor on the site,
//! so everything under `docs/` is already covered and is deliberately not
//! checked twice here. What it cannot see is the other half of this
//! repository's prose — `README.md`, `CLAUDE.md`, the changeset notes, the
//! issue templates, the chart README — which links into `docs/` constantly and
//! has nothing resolving those links at all. Rename a heading and the README's
//! deep link dies silently.
//!
//! Three questions, in the order a broken reference tends to arrive in:
//!
//! | Check | The failure it catches |
//! | --- | --- |
//! | the target exists | a page moved or was renamed |
//! | the target's name matches on disk exactly | a link written `Docs/` that works on this Mac and 404s on the Linux runner |
//! | a `#fragment` names a heading | the heading was reworded, which no filesystem can notice |
//!
//! Then one rule rather than a check, for the files that are published from
//! two directories at once. `docs/contributing.md` is a symlink to
//! `CONTRIBUTING.md`, so a relative link in it has to resolve from the
//! repository root *and* from `docs/`, and almost nothing does both. Those
//! files are held to absolute URLs.
//!
//! ```console
//! $ cargo run -p xtask -- links README.md CLAUDE.md
//! ```
//!
//! The file list comes from the Makefile, the same way [`prose`](mod@crate::prose)
//! takes one.

use std::collections::BTreeSet;
use std::path::Path;

use crate::util::{Failures, line_of, re, read, root};

/// Check every path given. Returns whether they all passed.
pub fn check(paths: &[&str]) -> bool {
    let mut f = Failures::default();
    let published = symlinked_into_docs();
    for path in paths {
        match read(path) {
            Ok(text) => check_one(path, &text, &published, &mut f),
            Err(e) => f.fail(e),
        }
    }
    if f.is_empty() {
        println!("links: {} pages, every reference resolves.", paths.len());
    }
    f.report("dead cross-references")
}

fn check_one(path: &str, text: &str, published: &BTreeSet<String>, f: &mut Failures) {
    let link = re(r"\]\(([^)\s]+)(?:\s+\x22[^\x22]*\x22)?\)");
    let dir = parent(path);
    for (at, target) in links_outside_fences(text, &link) {
        let at = format!("{path}:{}", line_of(text, at));
        if target.starts_with("http://") || target.starts_with("https://") {
            continue;
        }
        if published.contains(path) {
            f.fail(format!(
                "{at} links to {target:?}, which is relative\n    \
                 docs/{} is a symlink to this file, so every link in it is resolved \
                 from the repository root and from docs/ both. Use the \
                 https://miradb.dev/ URL instead.",
                path.to_lowercase()
            ));
            continue;
        }
        let (rel, anchor) = match target.split_once('#') {
            // A bare `#fragment` is a link into this same page.
            Some(("", a)) => (path.to_string(), Some(a)),
            Some((p, a)) => (join(&dir, p), Some(a)),
            None => (join(&dir, &target), None),
        };
        if let Err(e) = exists(&rel) {
            f.fail(format!("{at} links to {target:?}: {e}"));
            continue;
        }
        let (Some(anchor), true) = (anchor, rel.ends_with(".md")) else {
            continue;
        };
        // The page in hand rather than the one on disk, for the `#fragment`
        // case: they are the same file, and only one of them is what is being
        // checked.
        let page = if rel == path {
            text.to_string()
        } else if let Ok(t) = read(&rel) {
            t
        } else {
            continue;
        };
        if !headings(&page).contains(anchor) {
            f.fail(format!(
                "{at} links to {target:?} and {rel} has no heading with that slug\n    \
                 a heading was reworded. `grep -n '^#' {rel}` has the current list."
            ));
        }
    }
}

/// Every link target in `text`, with its byte offset, skipping fenced code.
///
/// A fence is where a link that is not a link lives: `mkdocs.yml` snippets,
/// `sed` one-liners and every example of the syntax this function parses.
fn links_outside_fences(text: &str, link: &regex::Regex) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut at = 0;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced {
            out.extend(link.captures_iter(line).map(|c| {
                let m = c.get(1).expect("group 1 is not optional");
                (at + m.start(), m.as_str().to_string())
            }));
        }
        at += line.len() + 1;
    }
    out
}

/// The anchor slugs a page's headings declare.
///
/// python-markdown's `toc`, which is what builds the site, in its own order:
/// inline markup dropped, then everything that is not a word character, a
/// space or a hyphen, then lower case, then **runs of hyphens and spaces
/// together** collapsed to one hyphen. That last step is the one worth
/// spelling out: `## 6.1 \`--offload\`: a copy` is `#61-offload-a-copy`, not
/// `#61---offload-a-copy`, and a slugifier that only collapses whitespace
/// reports the live link as dead. GitHub's differs in corners neither this
/// repository nor the check needs, because both ends of every link checked
/// here are files in this repository.
fn headings(page: &str) -> BTreeSet<String> {
    let heading = re(r"(?m)^#{1,6}[ \t]+(.*?)[ \t]*$");
    let markup = re(r"\[([^\]]*)\]\([^)]*\)|[`*_]");
    let strip = re(r"[^\w\s-]");
    let space = re(r"[-\s]+");
    let mut fenced = false;
    let mut out = BTreeSet::new();
    for line in page.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let Some(c) = heading.captures(line) else {
            continue;
        };
        let text = markup.replace_all(&c[1], "$1");
        let kept = strip.replace_all(&text, "");
        out.insert(space.replace_all(kept.trim(), "-").to_lowercase());
    }
    out
}

/// The path exists, spelled the way the link spells it.
///
/// `Path::exists` is not enough on macOS, where the filesystem is
/// case-insensitive: a link to `Docs/Index.md` opens here and 404s on the
/// Linux runner and on the published site. So every component is compared
/// against its directory's listing rather than asked of the kernel — every
/// one, because `Docs/market.md` is wrong in the half `rsplit` throws away.
fn exists(rel: &str) -> Result<(), String> {
    if !root().join(rel).exists() {
        return Err("no such file in this repository".into());
    }
    let mut dir = String::new();
    for name in rel.split('/') {
        let listed = std::fs::read_dir(root().join(&dir))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name() == name);
        if !listed {
            return Err(format!(
                "spelled {name:?}, which is not how it is spelled on disk; this Mac resolves \
                 it and the Linux runner will not. `ls {dir}` has the real name."
            ));
        }
        dir = join(&dir, name);
    }
    Ok(())
}

/// The repository-relative paths that a symlink under `docs/` points at.
fn symlinked_into_docs() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(&root().join("docs"), &mut |entry: &Path| {
        let Ok(target) = std::fs::read_link(entry) else {
            return;
        };
        let from = entry.parent().unwrap_or(Path::new("")).to_path_buf();
        if let Ok(rel) = from.join(target).strip_prefix(root()) {
            out.insert(join("", &rel.to_string_lossy()));
        }
    });
    out
}

fn walk(dir: &Path, each: &mut impl FnMut(&Path)) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
            each(&path);
        } else if path.is_dir() {
            walk(&path, each);
        }
    }
}

fn parent(rel: &str) -> String {
    rel.rsplit_once('/')
        .map_or(String::new(), |(d, _)| d.into())
}

/// `dir` and `rel` joined and normalised, with no filesystem access — a link
/// to a path that does not exist still has to be reported by name.
fn join(dir: &str, rel: &str) -> String {
    let mut out: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(text: &str) -> Vec<String> {
        let mut f = Failures::default();
        check_one("README.md", text, &BTreeSet::new(), &mut f);
        f.0
    }

    #[test]
    fn a_link_to_a_page_that_exists_passes_and_one_to_a_page_that_does_not_fails() {
        assert!(findings("[a](CLAUDE.md)").is_empty());
        let f = findings("[a](docs/architecture/nope.md)");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("README.md:1"), "{f:?}");
        assert!(f[0].contains("no such file"), "{f:?}");
    }

    /// The whole point of the anchor half: the path still resolves, so nothing
    /// else in the build can tell that the heading moved out from under it.
    #[test]
    fn an_anchor_that_names_no_heading_is_a_finding_and_the_real_one_is_not() {
        assert!(
            findings("[a](docs/architecture/corrections.md#01-what-is-not-true-yet)").is_empty()
        );
        let f = findings("[a](docs/architecture/corrections.md#01-what-was-not-true-yet)");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("no heading with that slug"), "{f:?}");
    }

    /// The first thing this check got wrong, and it got it wrong against a
    /// link that works: python-markdown collapses hyphens and spaces in one
    /// pass, so the flag in `6.1 \`--offload\`: a copy…` contributes no extra
    /// hyphens. A slugifier that collapses only whitespace calls the live
    /// anchor dead, which is the one failure mode a link gate cannot have.
    #[test]
    fn a_heading_with_punctuation_slugs_the_way_the_site_publishes_it() {
        let slugs = headings(
            "# 6. Retention worker\n\n## 6.1 `--offload`: a copy before the unlink\n\
             \n### 12.2.5 What the hop costs\n",
        );
        assert!(
            slugs.contains("61-offload-a-copy-before-the-unlink"),
            "{slugs:?}"
        );
        assert!(slugs.contains("1225-what-the-hop-costs"), "{slugs:?}");
    }

    #[test]
    fn a_fragment_with_no_path_is_read_against_the_page_it_is_on() {
        let f = findings("# Real heading\n\n[a](#real-heading) [b](#imagined-heading)\n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("imagined-heading"), "{f:?}");
    }

    /// Reproduces the case-insensitive filesystem: this passes `Path::exists`
    /// on the machine every one of these pages is written on.
    #[test]
    fn a_target_spelled_in_the_wrong_case_is_a_finding_even_on_a_mac() {
        let f = findings("[a](Docs/market.md)");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("Linux runner"), "{f:?}");
    }

    #[test]
    fn a_link_inside_a_fence_is_an_example_rather_than_a_link() {
        assert!(findings("```md\n[a](nope.md)\n```\n").is_empty());
    }

    /// A page published from two directories cannot have it both ways, and the
    /// four files this applies to are held to absolute URLs by hand today.
    #[test]
    fn a_relative_link_in_a_page_symlinked_into_docs_is_a_finding() {
        let published = BTreeSet::from(["CONTRIBUTING.md".to_string()]);
        let mut f = Failures::default();
        check_one("CONTRIBUTING.md", "[a](docs/market.md)", &published, &mut f);
        assert_eq!(f.0.len(), 1, "{:?}", f.0);
        assert!(f.0[0].contains("docs/contributing.md"), "{:?}", f.0);

        let mut ok = Failures::default();
        let absolute = "[a](https://miradb.dev/market/)";
        check_one("CONTRIBUTING.md", absolute, &published, &mut ok);
        assert!(ok.is_empty(), "{:?}", ok.0);
    }

    /// The set is derived from the tree rather than listed, so a sixth symlink
    /// is covered the day it is added.
    #[test]
    fn the_symlinked_pages_are_found_by_walking_docs() {
        let found = symlinked_into_docs();
        for expected in ["CONTRIBUTING.md", "MANIFESTO.md", "SECURITY.md"] {
            assert!(found.contains(expected), "{found:?}");
        }
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_page_it_is_written_on() {
        assert_eq!(join("docs/internals", "../market.md"), "docs/market.md");
        assert_eq!(join("docs/internals", "e2e.md"), "docs/internals/e2e.md");
        assert_eq!(join("", "README.md"), "README.md");
    }
}
