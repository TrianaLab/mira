//! What the cold tier would actually buy, on real blocks.
//!
//! §11 has a target of ≤0.35 bytes stored per byte of OTLP and a measurement of
//! 1.31. Compression is the obvious lever and the docs have been assuming it
//! works without saying by how much, which is the kind of claim this repo has
//! already been burned by once. So: rewrite real blocks with ZSTD, and report
//! the ratio and what the rewrite and the subsequent read cost.
//!
//! ```text
//! cargo run --release -p mira-core --example tier -- /tmp/mira-bench/logs
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use mira_core::block;

fn main() {
    let root: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("usage: tier <dir of block directories>");
            std::process::exit(2)
        })
        .into();

    let mut blocks: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    blocks.sort();
    // Enough to be representative without rewriting the whole corpus; the
    // spread across blocks is what matters, not the total.
    blocks.truncate(8);

    let tmp = std::env::temp_dir().join("mira-tier");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let (mut plain_total, mut zstd_total, mut lz4_total) = (0u64, 0u64, 0u64);
    let (mut write_ns, mut read_plain_ns, mut read_zstd_ns) = (0u128, 0u128, 0u128);
    let mut lz4_ns = 0u128;

    // LZ4_FRAME is the pure-Rust option — Arrow supports it and `lz4_flex` has
    // no C in it. It is here to be priced, not to be used: the decision it
    // settles is whether the ratio is worth one `cc` dependency.
    let mut by_table: std::collections::BTreeMap<String, (u64, u64, u64)> = Default::default();

    for b in &blocks {
        for f in std::fs::read_dir(b).unwrap().filter_map(|e| e.ok()) {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "arrow") {
                continue;
            }
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let plain = std::fs::metadata(&path).unwrap().len();

            let t = Instant::now();
            let table = block::open_table(&path).expect("open");
            let batch = table.batches.first().cloned().expect("one batch");
            read_plain_ns += t.elapsed().as_nanos();

            let out = tmp.join(format!("{name}.arrow"));
            let t = Instant::now();
            block::write_table_zstd(&out, &batch).expect("write");
            write_ns += t.elapsed().as_nanos();
            let z = std::fs::metadata(&out).unwrap().len();

            let t = Instant::now();
            let back = block::open_table(&out).expect("reopen");
            assert_eq!(back.batches[0].num_rows(), batch.num_rows());
            read_zstd_ns += t.elapsed().as_nanos();

            let out4 = tmp.join(format!("{name}.lz4.arrow"));
            let t = Instant::now();
            block::write_table_lz4(&out4, &batch).expect("write lz4");
            lz4_ns += t.elapsed().as_nanos();
            let l = std::fs::metadata(&out4).unwrap().len();

            plain_total += plain;
            zstd_total += z;
            lz4_total += l;
            let e = by_table.entry(name).or_default();
            e.0 += plain;
            e.1 += z;
            e.2 += l;
        }
    }

    let row = |name: &str, p: u64, z: u64, l: u64| {
        println!(
            "{name:<28} {:>12} {:>12} {:>7.3} {:>12} {:>7.3}",
            mib(p),
            mib(z),
            z as f64 / p as f64,
            mib(l),
            l as f64 / p as f64
        );
    };
    println!(
        "{:<28} {:>12} {:>12} {:>7} {:>12} {:>7}",
        "table", "plain", "zstd", "ratio", "lz4", "ratio"
    );
    for (name, (p, z, l)) in &by_table {
        row(name, *p, *z, *l);
    }
    row(
        &format!("TOTAL ({} blocks)", blocks.len()),
        plain_total,
        zstd_total,
        lz4_total,
    );
    println!();
    println!("read  plain: {:>8.2} s", read_plain_ns as f64 / 1e9);
    println!("read  zstd:  {:>8.2} s", read_zstd_ns as f64 / 1e9);
    let rate = |ns: u128| (plain_total as f64 / (1 << 20) as f64) / (ns as f64 / 1e9);
    println!(
        "zstd write:  {:>8.2} s | {:>8.1} MiB/s",
        write_ns as f64 / 1e9,
        rate(write_ns)
    );
    println!(
        "lz4  write:  {:>8.2} s | {:>8.1} MiB/s",
        lz4_ns as f64 / 1e9,
        rate(lz4_ns)
    );

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = Path::new("");
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / (1 << 20) as f64)
}
