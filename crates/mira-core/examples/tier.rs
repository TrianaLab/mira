//! Prices the cold tier on real blocks.
//!
//! The compression ratios in the README and in `docs/MARKET.md` are measured,
//! and this is what measures them. Point it at a data directory and it rewrites
//! every table in it three ways — uncompressed, ZSTD at the level
//! `block::ZSTD_LEVEL` sets, and LZ4_FRAME — using the engine's own writer, not
//! an approximation of it. Nothing is modified: every rewrite goes to a
//! temporary file that is unlinked before the next one.
//!
//! LZ4 is here because it is the pure-Rust alternative. `zstd-sys` is the only C
//! dependency in the tree and that is a stated property, so the question "what
//! would dropping it cost" has to have a number attached rather than an opinion,
//! and the number has to come from real telemetry rather than a corpus.
//!
//! ```sh
//! cargo run --release -p mira-core --example tier -- ./data
//! ```
//!
//! The denominator matters and there are two of them in circulation. This one
//! reports **engine-internal**: uncompressed Arrow column bytes against
//! compressed Arrow column bytes, which is the denominator ClickHouse,
//! VictoriaLogs and LogHouse publish against. The wire ratio — OTLP protobuf in
//! against bytes on disk — is a different and much less flattering number, and
//! `docs/MARKET.md` prints both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mira_core::block;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = match args.next() {
        Some(a) if a != "--help" && a != "-h" => PathBuf::from(a),
        _ => {
            eprintln!(
                "usage: tier <data-dir>\n\nprices the cold tier on every block under <data-dir>"
            );
            std::process::exit(2);
        }
    };

    // Keyed by `signal/table`, because both groupings are wanted and one key
    // gives both: every published ratio is quoted per signal, and the reason any
    // of them moved is always one table. Sorted, so two runs diff.
    let mut by_table: BTreeMap<String, [u64; 3]> = BTreeMap::new();
    let mut blocks = 0;
    // Wall clock inside `write_*`, per codec. The ratio is only half the
    // decision: compaction is a background sweep sharing cores with ingest, so a
    // level that buys a few percent of disk for several times the CPU is a bad
    // trade on an axis principle 1 also scores.
    let mut secs = [0f64; 3];
    // And wall clock inside `open_table` on what each codec produced. The design
    // assumed inflating a compressed buffer would cost more than the pages it
    // saves; that has to be a number, and this is the number. Page-cache-warm by
    // construction — the file was written microseconds earlier — which is the
    // *unfavourable* half of the comparison, because a warm read is exactly the
    // case where a plain block has nothing to fault and a compressed one still
    // has to inflate.
    let mut reads = [0f64; 3];
    let tmp = std::env::temp_dir().join(format!("mira-tier-{}", std::process::id()));

    for path in tables(&root) {
        let Ok(table) = block::open_table(&path) else {
            eprintln!("skipping unreadable {}", path.display());
            continue;
        };
        let Some(batch) = table.batches.first() else {
            continue;
        };
        blocks += 1;
        let row = by_table.entry(key(&root, &path)).or_default();
        for (slot, write) in [
            block::write_table as fn(&Path, &_) -> _,
            block::write_table_zstd,
            block::write_table_lz4,
        ]
        .into_iter()
        .enumerate()
        {
            let t0 = std::time::Instant::now();
            let ok = write(&tmp, batch).is_ok();
            secs[slot] += t0.elapsed().as_secs_f64();
            if ok {
                row[slot] += std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                let t0 = std::time::Instant::now();
                let back = block::open_table(&tmp);
                reads[slot] += t0.elapsed().as_secs_f64();
                assert!(back.is_ok(), "{} did not read back", tmp.display());
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);

    if blocks == 0 {
        eprintln!("no readable .arrow tables under {}", root.display());
        std::process::exit(1);
    }

    println!(
        "{:<30} {:>12} {:>12} {:>7} {:>12} {:>7}",
        "table", "plain", "zstd", "ratio", "lz4", "ratio"
    );
    let mut tot = [0u64; 3];
    let mut sub = [0u64; 3];
    let mut signal = String::new();
    for (name, cols) in &by_table {
        let this = name.split('/').next().unwrap_or_default();
        if this != signal {
            if !signal.is_empty() {
                line(&signal, sub[0], sub[1], sub[2]);
            }
            signal = this.to_owned();
            sub = [0; 3];
        }
        line(&format!("  {name}"), cols[0], cols[1], cols[2]);
        for (i, v) in cols.iter().enumerate() {
            sub[i] += v;
            tot[i] += v;
        }
    }
    line(&signal, sub[0], sub[1], sub[2]);
    println!("{:-<86}", "");
    line(&format!("{blocks} tables"), tot[0], tot[1], tot[2]);

    // Against the *plain* size in both cases: what the sweep has to chew through
    // is the uncompressed block, whatever comes out the other end.
    let mib = tot[0] as f64 / (1024.0 * 1024.0);
    println!(
        "\ncompressed at {:.0} MiB/s zstd, {:.0} MiB/s lz4, one core \
         ({:.1}s and {:.1}s of CPU for {mib:.0} MiB)",
        mib / secs[1],
        mib / secs[2],
        secs[1],
        secs[2],
    );
    println!(
        "read back in {:.1}s plain, {:.1}s zstd, {:.1}s lz4 (page-cache-warm, \
         {blocks} tables) — zstd is {:.2}x the plain read",
        reads[0],
        reads[1],
        reads[2],
        reads[1] / reads[0],
    );
}

/// `logs/log_attrs.arrow` from `<root>/logs/p=496975/<block>/log_attrs.arrow`.
/// A path that is not under a signal directory keeps its own leading component,
/// which is what makes this readable when pointed at a single block.
fn key(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    let signal = parts.next().unwrap_or_default();
    let table = rel.file_name().unwrap_or_default().to_string_lossy();
    format!("{signal}/{table}")
}

fn line(name: &str, plain: u64, zstd: u64, lz4: u64) {
    let ratio = |c: u64| if c == 0 { 0.0 } else { plain as f64 / c as f64 };
    println!(
        "{name:<30} {plain:>12} {zstd:>12} {:>6.2}x {lz4:>12} {:>6.2}x",
        ratio(zstd),
        ratio(lz4)
    );
}

/// Every `.arrow` file under `root`, at any depth: the layout is
/// `signal/p=<partition>/<block>/<table>.arrow` and this does not need to know
/// that.
fn tables(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "arrow") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
