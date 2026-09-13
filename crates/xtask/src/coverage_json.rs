//! The one number the README badge reads, taken off `cargo llvm-cov --json`.
//!
//! The badge holds no figure of its own — it is shields' `dynamic/json` reader
//! pointed at a file the docs deploy writes from the measurement that deploy
//! took. This is the writer. `make coverage-json` pipes llvm-cov's JSON in and
//! names the file and the commit.
//!
//! Floored to two decimals rather than rounded, for the same reason the ratchet
//! in the Makefile is: 99.999 is not 100, and a badge that says it is, is worse
//! than no badge. `commit` is not read by the badge; it is there so a figure
//! that looks wrong can be traced to the tree that produced it.

use std::io::Read;

use crate::util::parse_yaml;

pub fn run(out: &str, commit: &str) -> bool {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("error: reading llvm-cov JSON from stdin: {e}");
        return false;
    }
    // JSON is a subset of YAML 1.2, so the workflow parser reads this too. It
    // saves a second parser for one field.
    let doc = match parse_yaml(&text) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("error: llvm-cov did not answer with JSON: {e}");
            return false;
        }
    };
    let node = &doc["data"][0]["totals"]["lines"]["percent"];
    // llvm-cov writes `100` for a fully covered tree and `99.21…` otherwise, so
    // the field is an integer in exactly the case the float read would miss.
    let Some(percent) = node.as_f64().or_else(|| node.as_i64().map(|i| i as f64)) else {
        eprintln!(
            "error: no data[0].totals.lines.percent in llvm-cov's JSON. The shape \
             changed, or this was handed something else."
        );
        return false;
    };

    let floored = (percent * 100.0).floor() / 100.0;
    let body = format!("{{\"line\": {floored}, \"commit\": \"{commit}\"}}");
    if let Err(e) = std::fs::write(out, &body) {
        eprintln!("error: {out}: {e}");
        return false;
    }
    true
}
