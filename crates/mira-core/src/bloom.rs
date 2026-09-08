//! A block-level "might contain this id" filter.
//!
//! Block names prune by time, which answers every question except the one the
//! trace view asks. "Every span of trace `0000…6eaa`" has no time bound worth
//! having — you do not know when the trace happened, that is why you are
//! looking it up — so it reads every block on disk.
//!
//! A sidecar per block, written next to the Arrow tables at publish time and
//! read before any of them, turns that into one block opened. Measured over
//! 4.1 GB / 25M spans / 84 blocks, fetching one 8-span trace:
//!
//! ```text
//!            blocks scanned   rows scanned   cold     warm
//! without           84          25,000,000   14.3 s   14.3 s
//! with               1             212,992    250 ms    20 ms
//! ```
//!
//! The filters cost 65 KB per block — 5.3 MB against 4.1 GB, 0.13%.
//!
//! The two 64-bit halves of the id are the two hashes Kirsch-Mitzenmacher
//! double hashing wants — but each goes through a splitmix64 finalizer first,
//! and that is not optional. W3C only requires a trace id to be non-zero, and
//! plenty of real ones are structured rather than random: X-Ray puts an epoch
//! in the first four bytes, a counter-derived id leaves whole bytes constant.
//! The bit index is `(h1 + i*h2) & mask`, so it reads only the *low* bits of
//! the halves; ids that vary in their middle bytes would map every trace in a
//! block onto the same handful of bits and the filter would answer "maybe" to
//! everything. Five ops per half buys immunity to that.
//!
//! Everything here fails open. A short, corrupt or unrecognized file means
//! "scan the block", never "skip it" — a false positive costs one wasted block
//! read, a false negative silently loses spans from a trace.

use arrow_array::{Array, FixedSizeBinaryArray};

/// Filename inside a published block. Part of the on-disk format.
pub const TRACE_IDX: &str = "trace.idx";

const MAGIC: [u8; 4] = *b"MBLM";
const VERSION: u8 = 1;
/// magic 4 | version 1 | k 1 | pad 2 | words 4 | crc32 4
const HEADER: usize = 16;

/// Bits per inserted key. 10 with k=7 is the textbook ~0.8% false positive
/// rate.
const BITS_PER_KEY: usize = 10;
const K: u32 = 7;

/// Build a filter over a `FixedSizeBinary(16)` column. `None` when there is
/// nothing to index, in which case no file is written and the reader's
/// fail-open path scans the block.
///
/// The column holds one row per *span*, and a trace is eight of them. Sizing on
/// the row count would make every filter eight times bigger than the key set it
/// holds, and the read path pays that on every block it probes. Runs of the
/// same id collapse — spans of a trace arrive in one export and land adjacent —
/// which is a 16-byte compare per row against a hash set that would cost an
/// allocation and a probe. Interleaved traces just fall back to the row count,
/// which is the size we would have had anyway.
pub fn build(ids: &FixedSizeBinaryArray) -> Option<Vec<u8>> {
    if ids.value_length() != 16 {
        return None;
    }
    let keys = || Runs {
        ids,
        i: 0,
        prev: None,
    };
    let n = keys().count();
    if n == 0 {
        return None;
    }
    let words = ((n * BITS_PER_KEY).div_ceil(64)).next_power_of_two();
    let mut bits = vec![0u64; words];
    // A power-of-two word count makes the modulo an `and`, which matters: this
    // runs seven times per span on the seal path.
    let mask = (words as u64 * 64) - 1;

    for id in keys() {
        let (h1, h2) = halves(id);
        set(&mut bits, mask, h1, h2);
    }

    let mut out = Vec::with_capacity(HEADER + words * 8);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(K as u8);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(words as u32).to_le_bytes());
    let body: Vec<u8> = bits.iter().flat_map(|w| w.to_le_bytes()).collect();
    out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    out.extend_from_slice(&body);
    Some(out)
}

/// Could this block contain `id`? Anything unreadable answers yes.
pub fn may_contain(file: &[u8], id: &[u8; 16]) -> bool {
    if file.len() < HEADER || file[..4] != MAGIC || file[4] != VERSION {
        return true;
    }
    let k = file[5] as u32;
    let words = u32::from_le_bytes(file[8..12].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(file[12..16].try_into().unwrap());
    let body = &file[HEADER..];
    if k == 0 || !words.is_power_of_two() || body.len() != words * 8 {
        return true;
    }
    // The one check that costs something — 20 KB of CRC against a block read of
    // tens of megabytes. Skipping a block on the word of a corrupt filter is
    // the one outcome worth paying to avoid.
    if crc32fast::hash(body) != crc {
        return true;
    }

    let mask = (words as u64 * 64) - 1;
    let (h1, h2) = halves(id);
    for i in 0..k {
        let bit = h1.wrapping_add((i as u64).wrapping_mul(h2)) & mask;
        let word = u64::from_le_bytes(
            body[(bit as usize / 64) * 8..][..8]
                .try_into()
                .expect("slice of 8"),
        );
        if word & (1 << (bit % 64)) == 0 {
            return false;
        }
    }
    true
}

/// Non-null values, with adjacent duplicates dropped.
struct Runs<'a> {
    ids: &'a FixedSizeBinaryArray,
    i: usize,
    prev: Option<&'a [u8]>,
}

impl<'a> Iterator for Runs<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.i < self.ids.len() {
            let i = self.i;
            self.i += 1;
            if self.ids.is_null(i) {
                continue;
            }
            let v = self.ids.value(i);
            if self.prev != Some(v) {
                self.prev = Some(v);
                return Some(v);
            }
        }
        None
    }
}

fn halves(id: &[u8]) -> (u64, u64) {
    let h1 = mix(u64::from_le_bytes(id[..8].try_into().expect("16-byte id")));
    // Odd, so the probe sequence walks the whole filter instead of landing on
    // the same bit whenever the second half happens to be even.
    let h2 = mix(u64::from_le_bytes(
        id[8..16].try_into().expect("16-byte id"),
    )) | 1;
    (h1, h2)
}

/// splitmix64's finalizer: every input bit reaches every output bit.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn set(bits: &mut [u64], mask: u64, h1: u64, h2: u64) {
    for i in 0..K {
        let bit = h1.wrapping_add((i as u64).wrapping_mul(h2)) & mask;
        bits[bit as usize / 64] |= 1 << (bit % 64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::FixedSizeBinaryArray;

    fn id(n: u64) -> [u8; 16] {
        let mut b = [0u8; 16];
        // Not a counter: the filter's whole premise is that trace ids are
        // uniform, so a test over 0,1,2,… would measure a case that cannot
        // happen and hide a probe sequence that clusters.
        b[..8].copy_from_slice(&crate::identity::hash64(&n.to_le_bytes()).to_le_bytes());
        b[8..].copy_from_slice(&crate::identity::hash64(&(!n).to_le_bytes()).to_le_bytes());
        b
    }

    fn filter(n: u64) -> Vec<u8> {
        let ids: Vec<[u8; 16]> = (0..n).map(id).collect();
        let arr = FixedSizeBinaryArray::try_from_iter(ids.iter().map(|v| v.as_slice())).unwrap();
        build(&arr).unwrap()
    }

    /// Everything inserted must be found — a false negative is a lost trace —
    /// and the false positive rate has to be near the 0.8% the sizing promises,
    /// or the filter is a 20 KB file that skips nothing.
    #[test]
    fn no_false_negatives_and_few_false_positives() {
        let n = 10_000u64;
        let f = filter(n);
        for i in 0..n {
            assert!(may_contain(&f, &id(i)), "false negative at {i}");
        }
        let probes = 100_000u64;
        let fp = (n..n + probes).filter(|&i| may_contain(&f, &id(i))).count();
        let rate = fp as f64 / probes as f64;
        assert!(rate < 0.02, "false positive rate {rate}");
    }

    /// Every way the file can be wrong has to answer "scan it". This is the
    /// property the whole thing rests on: a filter is an optimization, and an
    /// optimization that can lose data is a bug.
    #[test]
    fn a_damaged_filter_says_maybe() {
        let f = filter(1_000);
        let probe = id(999_999);
        assert!(!may_contain(&f, &probe), "test needs a known-absent id");

        assert!(may_contain(&[], &probe), "empty");
        assert!(may_contain(&f[..HEADER - 1], &probe), "truncated header");
        assert!(may_contain(&f[..f.len() - 8], &probe), "truncated body");

        let mut bad = f.clone();
        bad[0] = b'X';
        assert!(may_contain(&bad, &probe), "wrong magic");

        let mut bad = f.clone();
        bad[4] = VERSION + 1;
        assert!(may_contain(&bad, &probe), "future version");

        // A single flipped bit in the bitmap is exactly the case a checksum
        // exists for: it turns a "no" into a wrong "no" with nothing else to
        // notice it.
        let mut bad = f.clone();
        bad[HEADER + 3] ^= 0x40;
        assert!(may_contain(&bad, &probe), "corrupt body");
    }

    /// The ids the load generator emits, and the shape every counter-derived or
    /// timestamp-prefixed id has: constant bytes at both ends, variation in the
    /// middle. Without the finalizer every one of these lands on the same bits
    /// and the filter says "maybe" to everything — which is not a wrong answer,
    /// just a 20 KB file that skips nothing.
    #[test]
    fn structured_ids_still_spread() {
        let structured = |n: u64| {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&(n as u32 as u64).to_be_bytes());
            b[8..].copy_from_slice(&(0x5555_5555_5500_0000 | n).to_be_bytes());
            b
        };
        let ids: Vec<[u8; 16]> = (0..10_000u64).map(structured).collect();
        let arr = FixedSizeBinaryArray::try_from_iter(ids.iter().map(|v| v.as_slice())).unwrap();
        let f = build(&arr).unwrap();

        for i in 0..10_000u64 {
            assert!(may_contain(&f, &structured(i)), "false negative at {i}");
        }
        let probes = 100_000u64;
        let fp = (10_000..10_000 + probes)
            .filter(|&i| may_contain(&f, &structured(i)))
            .count();
        let rate = fp as f64 / probes as f64;
        assert!(rate < 0.02, "false positive rate {rate}");
    }

    /// Eight spans per trace is the ordinary shape of a block, and the filter
    /// has to be sized for the traces, not the spans — otherwise every probe on
    /// the read path pays 8x for nothing.
    #[test]
    fn adjacent_duplicates_do_not_inflate_the_filter() {
        let ids: Vec<[u8; 16]> = (0..1_000u64).flat_map(|n| [id(n); 8]).collect();
        let arr = FixedSizeBinaryArray::try_from_iter(ids.iter().map(|v| v.as_slice())).unwrap();
        let fanned = build(&arr).unwrap();
        assert_eq!(fanned.len(), filter(1_000).len());
        for i in 0..1_000u64 {
            assert!(may_contain(&fanned, &id(i)), "false negative at {i}");
        }
    }

    /// An empty column writes no file, and the reader treats a missing one as
    /// "scan the block" — the same path a block from an older writer takes.
    #[test]
    fn nothing_to_index_writes_nothing() {
        assert!(build(&FixedSizeBinaryArray::new_null(16, 0)).is_none());
        assert!(build(&FixedSizeBinaryArray::new_null(16, 100)).is_none());
    }
}
