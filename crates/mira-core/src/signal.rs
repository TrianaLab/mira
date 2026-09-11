//! What logs, traces and metrics have in common on the write path.
//!
//! Very little, as it turns out, and that is the point of this being a small
//! trait rather than a framework. The three signals share no columns and no
//! tables; what they share is the *lifecycle* — accumulate rows across many
//! export requests, answer "would the next request overflow a dictionary",
//! answer "how big are you", and seal into a set of named Arrow tables plus a
//! time range.
//!
//! That lifecycle is the flusher's state machine in `mira::pipeline`, which is
//! about a hundred lines of genuinely subtle code: deferred jobs carried into
//! the next block, a deadline that starts at the first row rather than the last
//! flush, ordering preserved across a mid-batch seal, a builder replaced rather
//! than reused after a failed `finish`. Three hand-copied versions of that would
//! be three places for the next bug in it to be fixed once and missed twice.
//! One trait and one generic flusher is the cheaper of the two.
//!
//! Deliberately *not* in here: anything about columns, attributes or schemas.
//! Every signal's layout is its own, because OTLP's are.

use arrow_array::RecordBatch;

use crate::Result;

/// A block's worth of rows, sealed and ready for [`crate::block::publish`].
///
/// The table names are the file stems inside the published directory
/// (`logs.arrow`, `log_attrs.arrow`, …), so they are part of the on-disk format
/// and not a display detail.
pub struct Sealed {
    pub tables: Vec<(&'static str, RecordBatch)>,
    /// Opaque files published alongside the tables, by name.
    ///
    /// Indexes over what a block contains, for the questions the block name
    /// cannot answer — see [`crate::bloom`]. The read path treats a missing
    /// sidecar as "no information", so an old block or a signal that publishes
    /// none costs nothing but a scan.
    pub sidecars: Vec<(&'static str, Vec<u8>)>,
    /// Oldest and newest row. These become the block's directory name, which is
    /// how the read path prunes without opening a file.
    pub min_ts: i64,
    pub max_ts: i64,
    /// Rows in the root table. Reported, not derived from `tables`, because
    /// "how many spans" is not "how many rows in the biggest table".
    pub num_rows: usize,
}

impl Sealed {
    /// Assemble a sealed block, deriving the sidecars that every signal gets.
    ///
    /// The attribute filter and the zone map are built here rather than in each
    /// signal's `seal` for one reason: a signal that forgets to build one is
    /// merely slow, but a signal that grows a new attribute table and forgets to
    /// *include* it publishes an index that omits real values, and the read path
    /// then skips blocks that hold matching rows. Deriving them from the tables,
    /// once, makes that class of mistake unavailable — and the zone map needs it
    /// even more than the filter does, since a key missing from it is read as
    /// "this block has no such value" rather than as a false negative in a
    /// probabilistic structure.
    ///
    /// Also where the empty-block timestamp sentinels are normalized, which was
    /// three copies of the same pair of `if`s, and where the range is clamped
    /// non-negative. That clamp is the second half of `logs::nanos` — not a
    /// link, because it is `pub(crate)` and rustdoc will not resolve one:
    /// the encoders keep an out-of-range OTLP timestamp from ever becoming a
    /// negative one, and this is the single funnel all three of them seal
    /// through, so it is the cheapest place to guarantee that
    /// [`crate::block::publish`] and `block::scan` cannot disagree. They would:
    /// `dir_name` formats a negative with a leading `-` and `parse_dir_name`
    /// splits on `-`, so a block with a negative `min_ts` is published, acked
    /// as durable, and then invisible to every query and to retention forever.
    pub fn new(
        num_rows: usize,
        tables: Vec<(&'static str, RecordBatch)>,
        min_ts: i64,
        max_ts: i64,
    ) -> Sealed {
        Sealed::with(Sidecars::Build, num_rows, tables, min_ts, max_ts)
    }

    /// As [`Sealed::new`], but `Sidecars::Skip` leaves them out.
    ///
    /// Only [`SignalBuilder::snapshot`] skips them, and only because they are
    /// the expensive half of a seal — `attrs::index` and `zone::index` walk
    /// every attribute row, which the encode bench measures at roughly three
    /// times the cost of appending that row in the first place. A snapshot has
    /// no directory to publish them into and is always scanned, so building
    /// them would be pure waste repeated on every idle tick.
    pub fn with(
        sidecars: Sidecars,
        num_rows: usize,
        tables: Vec<(&'static str, RecordBatch)>,
        min_ts: i64,
        max_ts: i64,
    ) -> Sealed {
        let mut built = Vec::new();
        if sidecars == Sidecars::Build {
            if let Some(b) = crate::attrs::index(&tables) {
                built.push((crate::bloom::ATTR_IDX, b));
            }
            if let Some(b) = crate::zone::index(&tables) {
                built.push((crate::zone::ZONE_IDX, b));
            }
        }
        let sidecars = built;
        Sealed {
            num_rows,
            tables,
            sidecars,
            min_ts: if min_ts == i64::MAX { 0 } else { min_ts.max(0) },
            max_ts: if max_ts == i64::MIN { 0 } else { max_ts.max(0) },
        }
    }

    /// Attach a signal-specific sidecar. `None` writes nothing, which the reader
    /// reads as "no information about this block" — which is also how
    /// [`Sidecars::Skip`] gets away with omitting all of them.
    pub fn with_sidecar(mut self, name: &'static str, bytes: Option<Vec<u8>>) -> Sealed {
        if let Some(b) = bytes {
            self.sidecars.push((name, b));
        }
        self
    }

    pub fn table(&self, name: &str) -> Option<&RecordBatch> {
        self.tables.iter().find(|(n, _)| *n == name).map(|(_, b)| b)
    }
}

/// Whether a seal derives the pruning sidecars, or skips them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sidecars {
    Build,
    Skip,
}

/// A snapshot of the open, unsealed block: rows that have been acknowledged but
/// not yet published, in the same Arrow shape the read path already knows.
///
/// This is the read-your-writes repair the write-ahead log made necessary
/// (ARCHITECTURE section 4). With the log on, an export is acknowledged when its bytes
/// reach the page cache, which is well before the block that will hold them is
/// sealed and renamed into place — so without this, a client that just got a
/// `200` and queried immediately would see nothing for up to `max_block_age`.
///
/// `seq` is the sequence the block *will* publish under, and that is what makes
/// the whole thing cheap. A snapshot holds the builder's rows from row zero, in
/// the order they will be written, so row `n` of the snapshot is row `n` of the
/// eventual block. A cursor handed out over open data is therefore still exactly
/// correct after the seal, and the read path can drop the snapshot the moment a
/// real block turns up under the same `(node, seq)` — no rebasing, no dedupe by
/// content, no second identity for a row.
pub struct Open {
    pub node: u32,
    pub seq: u64,
    pub sealed: Sealed,
}

/// One signal's accumulator, from the flusher's point of view.
pub trait SignalBuilder: Default + Send + 'static {
    /// The decoded OTLP export request this signal accepts.
    type Request: Send + 'static;

    /// Directory component under the data dir. Also the block name prefix, so
    /// changing it is an on-disk format change.
    const SIGNAL: &'static str;

    /// Whether `req` is guaranteed to fit without overflowing a `UInt16`
    /// dictionary or id space.
    ///
    /// Must be conservative: an Arrow builder cannot be rolled back, so the
    /// flusher relies on a `true` here meaning [`Self::append_request`] will not
    /// fail on capacity. Answering `false` unnecessarily costs a slightly small
    /// block; answering `true` wrongly wedges the node.
    fn has_headroom_for(&self, req: &Self::Request) -> bool;

    /// Absorb one export request, returning the number of root-table rows added.
    fn append_request(&mut self, req: &Self::Request) -> Result<usize>;

    /// Rough resident cost, with variable-width heaps measured rather than
    /// estimated from row counts — a 32 KB GenAI prompt must not weigh the same
    /// as a 20-byte one.
    fn approx_bytes(&self) -> usize;

    fn is_empty(&self) -> bool;

    /// Seal and reset. After this returns, `self` is a fresh builder — including
    /// on the error path, where the caller has no way to know how far through
    /// the column-by-column finish it got.
    fn finish(&mut self) -> Result<Sealed>;

    /// Materialise the rows accumulated so far *without* resetting, for
    /// [`Open`]. Sidecar-free; see [`Sealed::with`].
    ///
    /// Costs one buffer copy per column, so the flusher only calls it when its
    /// channel has drained — under sustained ingest the channel never empties,
    /// the snapshot never runs, and the read-your-writes window closes on its
    /// own because a busy block seals in well under `max_block_age`.
    fn snapshot(&self) -> Result<Sealed>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant the block directory name cannot express. Every encoder
    /// clamps per row already; this asserts the funnel underneath them, because
    /// a negative that gets this far is a block nothing can read or delete.
    #[test]
    fn the_sealed_range_is_never_negative() {
        let s = Sealed::new(1, vec![], -1, -1);
        assert_eq!((s.min_ts, s.max_ts), (0, 0));
        let s = Sealed::new(1, vec![], i64::MIN, 5_000);
        assert_eq!((s.min_ts, s.max_ts), (0, 5_000));
        // ...and the empty-block sentinels still normalize to a zero range.
        let s = Sealed::new(0, vec![], i64::MAX, i64::MIN);
        assert_eq!((s.min_ts, s.max_ts), (0, 0));
        assert!(s.sidecars.is_empty(), "no attribute table, no filter");
    }
}
