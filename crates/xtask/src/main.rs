//! The checks and generators the build runs, in the language the build is in.
//!
//! These were three Python scripts and a handful of `python3 -c` one-liners in
//! the Makefile. They read Rust source, Markdown and workflow YAML and fail the
//! build when the tree stops matching what it claims — which makes them part of
//! the product's correctness story, and a part written in a language nothing
//! else here is written in. Nobody runs clippy over a shell heredoc. Nobody
//! notices when a regex in a script stops matching, because a check that
//! matches nothing passes forever.
//!
//! So they live here, where `cargo fmt`, `cargo clippy -D warnings` and
//! `cargo doc -D warnings` already run, and where "this pattern found nothing"
//! is a test somebody can run.
//!
//! ```console
//! $ cargo run -p xtask -- <subcommand>
//! ```
//!
//! | Subcommand | Was | Does |
//! | --- | --- | --- |
//! | [`ci`] | `scripts/check_ci.py` | the workflow graph is what everyone assumes |
//! | [`drift`] | `scripts/check_drift.py` | the numbers the README promises are still true |
//! | `drift --bump X.Y.Z` | `check_drift.py --bump` | rewrite every version site at once |
//! | [`reference`](mod@reference) | `scripts/gen_reference.py` | regenerate the reference pages from the code |
//! | [`coverage_json`] | a `python -c` in the Makefile | the badge's figure, floored |
//! | `parse-json` | a `python -c` in the Makefile | a JSON file parses |
//!
//! Every subcommand exits 0 or 1 and explains itself on the way out; none of
//! them takes a flag that changes what "pass" means.

mod ci;
mod coverage_json;
mod drift;
mod reference;
mod util;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let ok = match (args.first().map(String::as_str), rest.as_slice()) {
        (Some("ci"), []) => ci::run(),
        (Some("drift"), []) => drift::check(),
        (Some("drift"), ["--bump", to]) => drift::bump(to),
        (Some("reference"), []) => reference::run(),
        (Some("coverage-json"), [out, commit]) => coverage_json::run(out, commit),
        // Not a check of anything clever: `helm template` loads
        // `values.schema.json` on every render, so a syntax error in it makes
        // the negative tests in `make helm-schema` pass for the wrong reason.
        // This asks the question directly, before those run.
        (Some("parse-json"), [path]) => match util::read(path).and_then(|t| util::parse_yaml(&t)) {
            Ok(_) => {
                println!("{path} parses.");
                true
            }
            Err(e) => {
                eprintln!("error: {path}: {e}");
                false
            }
        },
        _ => {
            eprintln!(
                "usage: cargo run -p xtask -- <command>\n\
                 \n\
                 \x20 ci                        the workflow graph can block a merge\n\
                 \x20 drift                     the numbers the docs promise are measured\n\
                 \x20 drift --bump X.Y.Z        rewrite every version site\n\
                 \x20 reference                 regenerate the reference pages\n\
                 \x20 coverage-json OUT COMMIT  llvm-cov JSON on stdin -> the badge's file\n\
                 \x20 parse-json PATH           that file is valid JSON"
            );
            return ExitCode::FAILURE;
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
