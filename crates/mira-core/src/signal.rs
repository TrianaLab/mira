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
    /// cannot answer. Only traces has one today — see [`crate::bloom`] — and
    /// the read path treats a missing sidecar as "no information", so an old
    /// block or a signal that publishes none costs nothing.
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
    /// Borrowed view in the shape [`crate::block::publish`] wants.
    pub fn refs(&self) -> Vec<(&str, &RecordBatch)> {
        self.tables.iter().map(|(n, b)| (*n, b)).collect()
    }

    pub fn table(&self, name: &str) -> Option<&RecordBatch> {
        self.tables.iter().find(|(n, _)| *n == name).map(|(_, b)| b)
    }
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
}
