//! The four things every subcommand needs: the repository root, a file, a
//! regex, and somewhere to put a complaint.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use yaml_rust2::{Yaml, YamlLoader};

/// The workspace root, found by walking up from the working directory.
///
/// Not `CARGO_MANIFEST_DIR`: that is this crate's directory on the machine the
/// binary was *compiled* on, which is the wrong answer the moment the tool is
/// run against a different checkout — and every path below is relative to the
/// tree being checked, not to the tree that built the checker.
pub fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut dir = std::env::current_dir().expect("a working directory");
        loop {
            // The member manifests have no `[workspace]`, so this cannot stop
            // one level too early inside `crates/`.
            if std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|t| t.lines().any(|l| l.trim_end() == "[workspace]"))
            {
                return dir;
            }
            assert!(
                dir.pop(),
                "no Cargo.toml with a [workspace] section above {}: run this from \
                 inside the repository",
                std::env::current_dir().unwrap_or_default().display()
            );
        }
    })
}

/// Read a repository-relative path.
pub fn read(rel: &str) -> Result<String, String> {
    let path = root().join(rel);
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read a repository-relative path, or die saying which one.
///
/// Used where the file's absence is not a finding but a broken invocation — a
/// missing `README.md` is not drift, it is the wrong directory.
pub fn read_or_exit(rel: &str) -> String {
    read(rel).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    })
}

/// A compiled regex, or a panic naming the pattern.
///
/// Every pattern in this crate is a literal written by hand, so a compile error
/// is a bug in the source and not an input to handle. Panicking beats
/// threading a `Result` through every call site to report something that can
/// only happen once, at the top of a run, to the person who just typed it.
pub fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern:?}: {e}"))
}

/// Parse one YAML (or JSON — it is a subset) document.
pub fn parse_yaml(text: &str) -> Result<Yaml, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
    docs.into_iter()
        .next()
        .ok_or_else(|| "empty document".to_string())
}

/// The three glob shapes this crate needs, and no more: `crates/*/src/*.rs`,
/// `crates/mira/src/*.rs` and `docs/**/*.md`.
///
/// A glob crate would be a dependency for three call sites. The grammar is one
/// `*` directory component, an optional trailing `**`, and an extension;
/// anything else should be spelled out rather than added here.
pub fn glob(pattern: &str) -> Vec<String> {
    let (head, ext) = pattern.rsplit_once("/*").expect("a trailing /*<ext>");
    let recursive = head.ends_with("/**");
    let head = head.trim_end_matches("/**");
    let mut dirs = vec![String::new()];
    for segment in head.split('/') {
        dirs = dirs
            .into_iter()
            .flat_map(|prefix| expand(&prefix, segment))
            .collect();
    }
    let mut out = Vec::new();
    for dir in dirs {
        walk(&dir, ext, recursive, &mut out);
    }
    out
}

fn expand(prefix: &str, segment: &str) -> Vec<String> {
    let joined = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };
    if segment != "*" {
        return vec![joined(segment)];
    }
    let mut out: Vec<String> = std::fs::read_dir(root().join(prefix))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some(joined(e.file_name().to_str()?)))
        .collect();
    out.sort();
    out
}

fn walk(dir: &str, ext: &str, recursive: bool, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root().join(dir)) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let rel = format!("{dir}/{name}");
        if e.path().is_dir() {
            if recursive {
                walk(&rel, ext, recursive, out);
            }
        } else if name.ends_with(ext) {
            out.push(rel);
        }
    }
}

/// The 1-indexed line number containing byte offset `at`.
pub fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// Collected complaints, printed together at the end of a run.
///
/// One at a time would be worse for the reader it is written for: three
/// half-updated sites is one mistake, and finding out about them one push at a
/// time is three round trips through CI.
#[derive(Default)]
pub struct Failures(pub(crate) Vec<String>);

impl Failures {
    pub fn fail(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Print everything collected; return whether the run passed.
    pub fn report(&self, what: &str) -> bool {
        if self.0.is_empty() {
            return true;
        }
        eprintln!("\n{} {what}:\n", self.0.len());
        for f in &self.0 {
            eprintln!("  - {f}\n");
        }
        false
    }
}
