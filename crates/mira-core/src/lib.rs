//! Mira storage engine.
//!
//! Three layers, in dependency order:
//!
//! * [`schema`] — the OTAP-shaped Arrow schemas that are simultaneously the
//!   in-memory and the on-disk layout. There is no translation step.
//! * [`identity`] — the stable entity key that correlation joins on.
//! * [`attrs`] — the attribute tables and Resource-Scope preamble every signal
//!   shares.
//! * [`signal`] — the one shape every signal's encoder presents to the flusher.
//! * [`logs`] / [`traces`] / [`metrics`] — OTLP protobuf into those schemas,
//!   with block-local id rebasing.
//! * [`block`] — atomic publish of immutable block directories and zero-copy
//!   mmap reads back out of them.
//!
//! See `docs/ARCHITECTURE.md` for why each of those is shaped the way it is.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod attrs;
pub mod block;
pub mod bloom;
pub mod error;
pub mod frame;
pub mod identity;
pub mod json;
pub mod logs;
pub mod metrics;
pub mod query;
pub mod schema;
pub mod series;
pub mod signal;
pub mod traces;
pub mod wal;
pub mod zone;

pub use error::{Error, Result};
pub use signal::{Sealed, SignalBuilder};

/// Backs [`degraded_syncs`].
static DEGRADED_SYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many syncs have fallen back to `fsync(2)`.
///
/// Zero on a volume that implements write barriers, which is every volume Mira
/// is meant to run on. Non-zero means the durability promise on this node is
/// weaker than the one [`sync_all`] documents, and it is reported by
/// `/api/v1/stats` so that is a fact an operator can see rather than infer.
pub fn degraded_syncs() -> u64 {
    DEGRADED_SYNCS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `File::sync_all`, with the fallback Apple targets need and `std` does not do.
///
/// On an Apple target both `sync_all` and `sync_data` compile to
/// `fcntl(F_FULLFSYNC)` — std's `os_fsync`/`os_datasync` in `sys/fs/unix.rs`,
/// with no error handling around the `fcntl` beyond `cvt_r`. That is the
/// correct barrier and it is why an ack is not an fsync (section 4), but not
/// every filesystem implements it: a Docker Desktop bind mount is FUSE and
/// answers `ENOTSUP`, and some network volumes answer `EINVAL`. std turns
/// either into an error, so a publish that a plain `fsync(2)` would have
/// satisfied fails instead, and the node goes unready over a volume that works.
///
/// So: try the barrier, and on exactly those two errnos degrade to `fsync` and
/// count it. Counting is the whole point — a fallback nobody can see is silent
/// durability loss, which is worse than the failure it replaces.
pub fn sync_all(file: &std::fs::File) -> std::io::Result<()> {
    sync_with(file, std::fs::File::sync_all)
}

/// `File::sync_data`, degrading the same way [`sync_all`] does.
pub fn sync_data(file: &std::fs::File) -> std::io::Result<()> {
    sync_with(file, std::fs::File::sync_data)
}

/// The seam the two above share, and the one a test can drive: `strong` is the
/// barrier that might not be supported, and there is no way to make a real
/// filesystem refuse `F_FULLFSYNC` on demand inside a unit test.
fn sync_with(
    file: &std::fs::File,
    strong: impl Fn(&std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match strong(file) {
        Ok(()) => Ok(()),
        Err(e) => degrade(file, e),
    }
}

#[cfg(target_vendor = "apple")]
fn degrade(file: &std::fs::File, e: std::io::Error) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if !matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
        return Err(e);
    }
    // SAFETY: `file` owns the descriptor and outlives the call.
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    DEGRADED_SYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Everywhere else `sync_all` is already `fsync`, so there is nothing to
/// degrade to and an error is an error.
#[cfg(not(target_vendor = "apple"))]
fn degrade(_file: &std::fs::File, e: std::io::Error) -> std::io::Result<()> {
    Err(e)
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use std::io::Error;

    /// One test, not three, because the counter is process-global and
    /// `cargo test` runs a crate's tests on many threads: three tests each
    /// asserting a delta on one counter is a race, and the first draft of this
    /// module was exactly that. No `tempfile` either — the dependency budget is
    /// a product property (`docs/ARCHITECTURE.md` section 11) for one file.
    #[test]
    fn the_barrier_runs_and_only_enotsup_degrades() {
        let path = std::env::temp_dir().join(format!("mira-sync-{}", std::process::id()));
        let file = std::fs::File::create(&path).expect("create");

        // A real volume has the barrier, so nothing degrades.
        sync_all(&file).expect("sync_all");
        sync_data(&file).expect("sync_data");
        assert_eq!(degraded_syncs(), 0, "a real volume degrades nothing");

        // EIO is a write that failed, not a feature that is missing.
        let e = sync_with(&file, |_| Err(Error::from_raw_os_error(libc::EIO)))
            .expect_err("EIO must not be swallowed");
        assert_eq!(e.raw_os_error(), Some(libc::EIO));

        // No errno at all — a `std` error that never came from the kernel.
        let e = sync_with(&file, |_| Err(Error::other("synthetic")))
            .expect_err("no errno, no fallback");
        assert!(e.raw_os_error().is_none());
        assert_eq!(degraded_syncs(), 0, "neither of those is a missing barrier");

        // The whole point of the wrapper, and the only way to reach the
        // fallback: `strong` refuses the way a FUSE mount refuses, `fsync(2)`
        // on the real descriptor underneath still works, and the counter says
        // it happened. Elsewhere `sync_all` is already `fsync`, so there is
        // nothing to degrade to and every errno stays an error.
        for (i, errno) in [libc::ENOTSUP, libc::EINVAL].into_iter().enumerate() {
            let r = sync_with(&file, |_| Err(Error::from_raw_os_error(errno)));
            if cfg!(target_vendor = "apple") {
                r.unwrap_or_else(|e| panic!("errno {errno} should have degraded, got {e}"));
                assert_eq!(degraded_syncs(), i as u64 + 1);
            } else {
                assert_eq!(
                    r.expect_err("no fallback off Apple").raw_os_error(),
                    Some(errno)
                );
                assert_eq!(degraded_syncs(), 0);
            }
        }

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, StringArray, UInt32Array, UInt64Array};
    use mira_proto::collector::logs::v1::ExportLogsServiceRequest;
    use mira_proto::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value::Value};
    use mira_proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use mira_proto::resource::v1::Resource;

    fn kv(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(v.into())),
            }),
        }
    }

    fn request(service: &str, n: usize, base_ts: u64) -> ExportLogsServiceRequest {
        request_from(service, &[], n, base_ts)
    }

    /// `extra` carries non-identifying resource attributes, so that a caller can
    /// simulate an exporter that starts reporting more about the same instance.
    fn request_from(
        service: &str,
        extra: &[KeyValue],
        n: usize,
        base_ts: u64,
    ) -> ExportLogsServiceRequest {
        let mut attributes = vec![
            kv("service.name", service),
            kv("service.instance.id", "7f3a"),
        ];
        attributes.extend_from_slice(extra);
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes,
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: "test".into(),
                        ..Default::default()
                    }),
                    log_records: (0..n)
                        .map(|i| LogRecord {
                            time_unix_nano: base_ts + i as u64,
                            severity_number: 9,
                            severity_text: "INFO".into(),
                            body: Some(AnyValue {
                                value: Some(Value::StringValue(format!("line {i}"))),
                            }),
                            attributes: vec![kv("http.method", "GET")],
                            trace_id: vec![7u8; 16].into(),
                            span_id: vec![3u8; 8].into(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// The load-bearing property of the whole engine: what we write, we can read
    /// back out of a mapping with zero buffer copies. If this fails, Mira is just
    /// another columnar store with an extra memcpy.
    #[test]
    fn roundtrip_is_zero_copy_and_prunable() {
        let root = std::env::temp_dir().join(format!("mira-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let mut b = logs::LogsBuilder::new();
        // Two identical requests from the same service: interned once.
        assert_eq!(
            b.append_request(&request("checkout", 500, 1_000)).unwrap(),
            500
        );
        assert_eq!(
            b.append_request(&request("checkout", 500, 2_000)).unwrap(),
            500
        );
        // Same instance, now reporting one more non-identifying attribute. A
        // second resource row, but it must NOT be a second entity.
        assert_eq!(
            b.append_request(&request_from(
                "checkout",
                &[kv("k8s.node.name", "node-4")],
                100,
                3_000
            ))
            .unwrap(),
            100
        );
        assert_eq!(
            b.append_request(&request("payments", 100, 3_100)).unwrap(),
            100
        );

        let sealed = b.finish().unwrap();
        assert_eq!(sealed.num_rows, 1200);
        assert_eq!(sealed.min_ts, 1_000);
        assert_eq!(sealed.max_ts, 3_199);
        // Every table the on-disk format promises, named exactly as its file.
        assert_eq!(
            sealed.tables.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            schema::LOGS_BLOCK_TABLES
        );
        let rows = |t: &str| sealed.table(t).unwrap().num_rows();
        // checkout(2 attrs) + checkout-with-node(3) + payments(2).
        assert_eq!(rows("resources"), 3);
        assert_eq!(rows("resource_attrs"), 7);
        assert_eq!(rows("scope_attrs"), 1);
        assert_eq!(rows("log_attrs"), 1200);

        // The load-bearing correlation invariant: attribute drift does not fork
        // an entity. Three resource rows, two entities.
        let keys = sealed
            .table("resources")
            .unwrap()
            .column_by_name("key")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(
            keys.value(0),
            keys.value(1),
            "attribute drift must not fork the entity"
        );
        assert_ne!(keys.value(0), keys.value(2));

        let node = block::node_id("replica-a");
        let published = block::publish(&root, "logs", node, 1, 0, &sealed).unwrap();
        assert_eq!(published.node, node);
        // A second replica publishing the same time range must not collide.
        assert_ne!(node, block::node_id("replica-b"));

        // Catalog comes back from directory names alone — no file opened.
        let catalog = block::scan(&root, "logs").unwrap();
        assert_eq!(catalog, vec![published.clone()]);
        assert!(catalog[0].overlaps(0, 1_500));
        assert!(!catalog[0].overlaps(10_000, 20_000));

        // Zero-copy read back. require_alignment(true) is on inside open_table,
        // so this call errors rather than silently copying.
        let logs = block::open_table(&published.dir.join("logs.arrow")).unwrap();
        assert_eq!(logs.batches.len(), 1);
        let rb = &logs.batches[0];
        assert_eq!(rb.num_rows(), 1200);

        // THE load-bearing invariant: every buffer of every column points into
        // the mapping. If this ever drops below n/n, Mira has silently become a
        // store that memcpies its whole working set on every read.
        let (inside, total) = logs.zero_copy_ratio();
        assert_eq!(inside, total, "{inside}/{total} buffers zero-copy");
        assert!(total >= 13, "expected one buffer per column at least");

        let ids = rb
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(ids.value(0), 0);
        assert_eq!(ids.value(1199), 1199, "ids must be dense and block-local");

        let body = rb
            .column_by_name("body")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(body.value(0), "line 0");

        // A join inside a block is safe precisely because ids were rebased.
        let attrs = block::open_table(&published.dir.join("log_attrs.arrow")).unwrap();
        let parents = attrs.batches[0]
            .column_by_name("parent_id")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(parents.value(1199), 1199);

        // Retention is an unlink of the directory.
        assert_eq!(block::expire(&root, "logs", 10_000).unwrap(), 1);
        assert!(block::scan(&root, "logs").unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The two inputs to the seal decision. Both were wrong in ways that only
    /// show up in production: a block sized by row count alone balloons on large
    /// bodies, and a dictionary discovered full mid-append leaves a half-written
    /// row that fails every subsequent flush.
    #[test]
    fn seal_decision_sees_bodies_and_dictionary_pressure() {
        let mut small = logs::LogsBuilder::new();
        small
            .append_request(&request("checkout", 100, 1_000))
            .unwrap();

        let prompt = "x".repeat(32 << 10);
        let mut req = request("checkout", 100, 1_000);
        for sl in &mut req.resource_logs[0].scope_logs {
            for r in &mut sl.log_records {
                r.body = Some(AnyValue {
                    value: Some(Value::StringValue(prompt.clone())),
                });
            }
        }
        let mut big = logs::LogsBuilder::new();
        big.append_request(&req).unwrap();
        // Same row count, same schema, three orders of magnitude apart. Counting
        // fixed-width columns only would make these two numbers equal.
        assert!(small.approx_bytes() < 64 << 10, "{}", small.approx_bytes());
        assert!(big.approx_bytes() > 3 << 20, "{}", big.approx_bytes());

        // A request wider than an empty block's dictionary is refused headroom
        // up front, not discovered part way through an append.
        let mut wide = request("checkout", 1, 1_000);
        wide.resource_logs[0].scope_logs[0].log_records[0].attributes = (0..=schema::DICT_CAP)
            .map(|i| kv(&format!("k{i}"), "v"))
            .collect();
        assert!(!small.has_headroom_for(&wide));
        assert!(small.has_headroom_for(&request("checkout", 10, 1)));
    }

    /// Spans are the only signal with grandchildren: an event belongs to a span
    /// and its attributes belong to the event. Both hops are block-local ids, and
    /// getting either wrong turns a join into a cross product that still returns
    /// plausible-looking rows.
    #[test]
    fn span_events_and_links_are_child_tables_with_their_own_ids() {
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::trace::v1::span::{Event, Link};
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, status::StatusCode};

        // Two spans, the second with two events and one link, so a bug that
        // parents events to the wrong span shows up as a wrong parent_id rather
        // than as a coincidentally-correct zero.
        let span = |i: u64, events: usize, links: usize| Span {
            trace_id: vec![1u8; 16].into(),
            span_id: vec![i as u8; 8].into(),
            name: "GET /checkout".into(),
            kind: 2,
            start_time_unix_nano: 10_000 + i,
            end_time_unix_nano: 10_000 + i + 500,
            attributes: vec![kv("http.method", "GET")],
            status: Some(Status {
                code: StatusCode::Error as i32,
                message: "boom".into(),
            }),
            events: (0..events)
                .map(|e| Event {
                    time_unix_nano: 10_100 + e as u64,
                    name: "exception".into(),
                    attributes: vec![kv("exception.type", "IOError")],
                    ..Default::default()
                })
                .collect(),
            // No attributes on the link — the common case, and it leaves
            // span_link_attrs empty, which the publish check below relies on.
            links: (0..links)
                .map(|_| Link {
                    trace_id: vec![9u8; 16].into(),
                    span_id: vec![8u8; 8].into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };

        let mut b = traces::TracesBuilder::new();
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![
                        kv("service.name", "checkout"),
                        kv("service.instance.id", "7f3a"),
                    ],
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "test".into(),
                        ..Default::default()
                    }),
                    spans: vec![span(0, 0, 0), span(1, 2, 1)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        assert!(b.has_headroom_for(&req));
        assert_eq!(b.append_request(&req).unwrap(), 2);

        let sealed = b.finish().unwrap();
        assert_eq!(sealed.num_rows, 2);
        assert_eq!(
            sealed.tables.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            schema::TRACES_BLOCK_TABLES
        );
        // The block's range spans start..end, not start..start: a query for the
        // instant a long span ended has to find it.
        assert_eq!(sealed.min_ts, 10_000);
        assert_eq!(sealed.max_ts, 10_501);

        let rows = |t: &str| sealed.table(t).unwrap().num_rows();
        assert_eq!(rows("span_events"), 2);
        assert_eq!(rows("span_links"), 1);
        assert_eq!(rows("span_event_attrs"), 2);
        assert_eq!(rows("span_link_attrs"), 0);
        assert_eq!(rows("span_attrs"), 2);

        let u32col = |t: &str, c: &str| {
            sealed
                .table(t)
                .unwrap()
                .column_by_name(c)
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap()
                .clone()
        };
        // Both events hang off span 1, and they carry ids 0 and 1 of their own —
        // the ids span_event_attrs.parent_id refers to. Sharing the span's id
        // here is the bug this test exists to catch.
        assert_eq!(u32col("span_events", "parent_id").values(), &[1, 1]);
        assert_eq!(u32col("span_events", "id").values(), &[0, 1]);
        assert_eq!(u32col("span_event_attrs", "parent_id").values(), &[0, 1]);
        assert_eq!(u32col("span_links", "parent_id").values(), &[1]);

        // An empty table costs ~1-2.5 KB of Arrow IPC framing and is not written.
        // A traces block has nine tables and a service that emits no span links
        // would otherwise pay for four of them on every seal.
        let root = std::env::temp_dir().join(format!("mira-tr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let published =
            block::publish(&root, "traces", block::node_id("a"), 1, 0, &sealed).unwrap();
        assert!(!published.dir.join("span_link_attrs.arrow").exists());
        assert!(
            block::open_table_opt(&published.dir.join("span_link_attrs.arrow"))
                .unwrap()
                .is_none()
        );
        let ev = block::open_table_opt(&published.dir.join("span_events.arrow"))
            .unwrap()
            .unwrap();
        let (inside, total) = ev.zero_copy_ratio();
        assert_eq!(inside, total, "{inside}/{total} buffers zero-copy");
        let _ = std::fs::remove_dir_all(&root);

        let spans = sealed.table("spans").unwrap();
        let durations = spans
            .column_by_name("duration_nano")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(durations.values(), &[500, 500]);

        // Links point out of the block, so their ids stay raw. Rebasing these
        // would silently repoint a link at a local row.
        let lt = sealed.table("span_links").unwrap();
        let lt = lt
            .column_by_name("trace_id")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(lt.value(0), &[9u8; 16]);
    }

    /// A span with a zero start time is malformed; folding it into the block's
    /// range would make the directory name claim to cover the epoch, and every
    /// temporal query would then have to open the block to find nothing.
    #[test]
    fn malformed_span_times_do_not_widen_the_block_range() {
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

        let mut b = traces::TracesBuilder::new();
        b.append_request(&ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![
                        Span {
                            name: "no-clock".into(),
                            ..Default::default()
                        },
                        Span {
                            name: "backwards".into(),
                            start_time_unix_nano: 5_000,
                            end_time_unix_nano: 4_000,
                            ..Default::default()
                        },
                        Span {
                            name: "fine".into(),
                            start_time_unix_nano: 6_000,
                            end_time_unix_nano: 6_100,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
        .unwrap();

        let sealed = b.finish().unwrap();
        assert_eq!(
            sealed.num_rows, 3,
            "malformed spans are stored, not dropped"
        );
        assert_eq!(sealed.min_ts, 5_000);
        assert_eq!(sealed.max_ts, 6_100);
        // end < start saturates to zero rather than wrapping to 584 years.
        let d = sealed.table("spans").unwrap();
        let d = d
            .column_by_name("duration_nano")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(d.values(), &[0, 0, 100]);
    }

    /// All five metric types through one request, because the layout's whole
    /// premise is that they go to different tables while sharing one point id
    /// space — and a shared counter is exactly the kind of thing that works for
    /// one type and silently collides for four.
    #[test]
    fn every_metric_type_lands_in_its_own_table_from_one_id_space() {
        use arrow_array::{Float64Array, ListArray, UInt8Array};
        use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
        use mira_proto::metrics::v1::metric::Data;
        use mira_proto::metrics::v1::number_data_point::Value as NumValue;
        use mira_proto::metrics::v1::summary_data_point::ValueAtQuantile;
        use mira_proto::metrics::v1::{
            AggregationTemporality, Exemplar, ExponentialHistogram, ExponentialHistogramDataPoint,
            Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
            ScopeMetrics, Sum, Summary, SummaryDataPoint,
            exponential_histogram_data_point::Buckets,
        };

        let number = |t: u64, v: i64| NumberDataPoint {
            attributes: vec![kv("route", "/checkout")],
            time_unix_nano: t,
            start_time_unix_nano: 1, // process start: must not widen the block
            value: Some(NumValue::AsInt(v)),
            ..Default::default()
        };
        // Two histogram points sharing one bounds array, so the interning is
        // actually exercised rather than merely present.
        let hist = |t: u64| HistogramDataPoint {
            time_unix_nano: t,
            count: 3,
            sum: Some(1.5),
            bucket_counts: vec![1, 1, 1],
            explicit_bounds: vec![0.1, 0.5],
            exemplars: vec![Exemplar {
                time_unix_nano: t,
                trace_id: vec![4u8; 16].into(),
                span_id: vec![5u8; 8].into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let metrics = vec![
            Metric {
                name: "http.server.request.count".into(),
                unit: "{request}".into(),
                data: Some(Data::Sum(Sum {
                    data_points: vec![number(1_000, 7), number(2_000, 9)],
                    aggregation_temporality: AggregationTemporality::Cumulative as i32,
                    is_monotonic: true,
                })),
                metadata: vec![kv("owner", "checkout-team")],
                ..Default::default()
            },
            Metric {
                name: "process.memory".into(),
                data: Some(Data::Gauge(Gauge {
                    data_points: vec![number(1_500, 42)],
                })),
                ..Default::default()
            },
            Metric {
                name: "http.server.duration".into(),
                data: Some(Data::Histogram(Histogram {
                    data_points: vec![hist(1_100), hist(2_100)],
                    aggregation_temporality: AggregationTemporality::Delta as i32,
                })),
                ..Default::default()
            },
            Metric {
                name: "rpc.duration".into(),
                data: Some(Data::ExponentialHistogram(ExponentialHistogram {
                    data_points: vec![ExponentialHistogramDataPoint {
                        time_unix_nano: 1_200,
                        count: 4,
                        scale: 3,
                        zero_count: 1,
                        positive: Some(Buckets {
                            offset: 2,
                            bucket_counts: vec![1, 2],
                        }),
                        ..Default::default()
                    }],
                    aggregation_temporality: AggregationTemporality::Delta as i32,
                })),
                ..Default::default()
            },
            Metric {
                name: "legacy.latency".into(),
                data: Some(Data::Summary(Summary {
                    data_points: vec![SummaryDataPoint {
                        time_unix_nano: 3_000,
                        count: 10,
                        sum: 5.0,
                        quantile_values: vec![
                            ValueAtQuantile {
                                quantile: 0.5,
                                value: 0.4,
                            },
                            ValueAtQuantile {
                                quantile: 0.99,
                                value: 0.9,
                            },
                        ],
                        ..Default::default()
                    }],
                })),
                ..Default::default()
            },
        ];

        let mut b = metrics::MetricsBuilder::new();
        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv("service.name", "checkout")],
                    ..Default::default()
                }),
                scope_metrics: vec![ScopeMetrics {
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        assert!(b.has_headroom_for(&req));
        // 3 number + 2 histogram + 1 exponential + 1 summary.
        assert_eq!(b.append_request(&req).unwrap(), 7);

        let sealed = b.finish().unwrap();
        assert_eq!(sealed.num_rows, 7);
        assert_eq!(
            sealed.tables.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            schema::METRICS_BLOCK_TABLES
        );
        let rows = |t: &str| sealed.table(t).unwrap().num_rows();
        assert_eq!(rows("metrics"), 5);
        assert_eq!(rows("number_dp"), 3);
        assert_eq!(rows("hist_dp"), 2);
        assert_eq!(rows("exp_hist_dp"), 1);
        assert_eq!(rows("summary_dp"), 1);
        assert_eq!(rows("exemplars"), 2);
        assert_eq!(rows("metric_attrs"), 1);
        // Two histogram points, one bounds row: the interning is the reason
        // hist_dp measured 1.67x smaller.
        assert_eq!(rows("hist_bounds"), 1);

        // start_time_unix_nano is process start and must not widen the block —
        // otherwise every cumulative metric makes its block match every query.
        assert_eq!(sealed.min_ts, 1_000);
        assert_eq!(sealed.max_ts, 3_000);

        let u32col = |t: &str, c: &str| {
            sealed
                .table(t)
                .unwrap()
                .column_by_name(c)
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap()
                .clone()
        };
        // THE invariant of this layout: one id space across four tables. Sum
        // takes 0-1, gauge 2, histogram 3-4, exponential 5, summary 6 — never
        // the same number twice, which is what lets dp_attrs and exemplars key
        // on a point without a table discriminant.
        assert_eq!(u32col("number_dp", "id").values(), &[0, 1, 2]);
        assert_eq!(u32col("hist_dp", "id").values(), &[3, 4]);
        assert_eq!(u32col("exp_hist_dp", "id").values(), &[5]);
        assert_eq!(u32col("summary_dp", "id").values(), &[6]);
        assert_eq!(u32col("exemplars", "parent_id").values(), &[3, 4]);
        // Only the three number points carry attributes here, and dp_attrs
        // points straight at them.
        assert_eq!(u32col("dp_attrs", "parent_id").values(), &[0, 1, 2]);
        // Both histogram points share bounds row 0.
        assert_eq!(u32col("hist_dp", "bounds_id").values(), &[0, 0]);

        // Temporality and monotonicity live on the descriptor, not on 300,000
        // points that would each repeat them.
        let kinds = sealed.table("metrics").unwrap();
        let kind = kinds
            .column_by_name("kind")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt8Array>()
            .unwrap();
        assert_eq!(kind.values(), &[2, 1, 3, 4, 5]);

        // Quantiles round-trip as two parallel lists.
        let sd = sealed.table("summary_dp").unwrap();
        let q = sd
            .column_by_name("quantile")
            .unwrap()
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap()
            .value(0);
        let q = q.as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(q.values(), &[0.5, 0.99]);

        // And the whole thing survives a publish/mmap round trip. A List column
        // has three buffers of its own; if any of them were copied, this drops
        // below n/n.
        let root = std::env::temp_dir().join(format!("mira-me-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let published =
            block::publish(&root, "metrics", block::node_id("a"), 1, 0, &sealed).unwrap();
        for t in ["number_dp", "hist_dp", "hist_bounds", "summary_dp"] {
            let m = block::open_table(&published.dir.join(format!("{t}.arrow"))).unwrap();
            let (inside, total) = m.zero_copy_ratio();
            assert_eq!(inside, total, "{t}: {inside}/{total} buffers zero-copy");
        }
        // Nothing in this request had exemplar attributes, so that table is
        // absent rather than 2.5 KB of framing.
        assert!(!published.dir.join("exemplar_attrs.arrow").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The read path end to end: publish two blocks, then filter on a resource
    /// attribute, a record attribute, a dictionary column and a raw field, and
    /// check that pruning actually skips work rather than merely returning the
    /// right rows by scanning everything.
    #[test]
    fn search_filters_across_attribute_levels_and_prunes_by_time() {
        use query::{Op, Search, Signal, Target, Term, Value as QV};

        let root = std::env::temp_dir().join(format!("mira-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Two blocks, two services, disjoint time ranges. Block 1 is older.
        for (seq, (service, base)) in [("checkout", 1_000u64), ("payments", 5_000)]
            .into_iter()
            .enumerate()
        {
            let mut b = logs::LogsBuilder::new();
            b.append_request(&request(service, 10, base)).unwrap();
            let sealed = b.finish().unwrap();
            block::publish(&root, "logs", block::node_id("a"), seq as u64, 0, &sealed).unwrap();
        }

        let q = |terms: Vec<Term>, from: i64, to: i64, limit: usize| Search {
            signal: Signal::Logs,
            from,
            to,
            terms,
            limit,
            after: None,
        };
        let attr = |k: &str, v: &str| Term {
            target: Target::Attr(k.into()),
            op: Op::Eq,
            value: QV::Str(v.into()),
        };
        let field = |c: &str, op: Op, v: QV| Term {
            target: Target::Field(c.into()),
            op,
            value: v,
        };

        // service.name is a *resource* attribute; the user does not know that
        // and should not have to. It resolves through resources -> resource_id.
        let r = query::search(
            &root,
            &q(vec![attr("service.name", "payments")], 0, i64::MAX, 100),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 10);
        assert!(r.json.contains("\"service.name\":\"payments\""));
        assert!(!r.json.contains("checkout"));
        // Time did not prune this — both blocks are in the window — but the
        // attribute filter did: the "checkout" block never carried the value, so
        // its sidecar ruled it out before it was opened.
        assert_eq!(r.stats.blocks_scanned, 1);

        // http.method is a *record* attribute, on every row of both blocks.
        let r = query::search(
            &root,
            &q(vec![attr("http.method", "GET")], 0, i64::MAX, 100),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 20);

        // A key that exists nowhere matches nothing rather than erroring.
        let r = query::search(&root, &q(vec![attr("nope", "x")], 0, i64::MAX, 100)).unwrap();
        assert_eq!(r.stats.rows_matched, 0);
        assert_eq!(r.json, "[]");

        // Time pruning happens on the directory name, before any file is
        // opened: the older block is never touched.
        let r = query::search(&root, &q(vec![], 5_000, 9_000, 100)).unwrap();
        assert_eq!(r.stats.blocks_total, 2);
        assert_eq!(
            r.stats.blocks_scanned, 1,
            "the 1_000-range block must prune"
        );
        assert_eq!(r.stats.rows_matched, 10);

        // Dictionary column, substring op: resolved once against the dictionary
        // and then matched on u16 codes.
        let r = query::search(
            &root,
            &q(
                vec![field("severity_text", Op::Contains, QV::Str("NF".into()))],
                0,
                i64::MAX,
                100,
            ),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 20);

        // A numeric field given as a string, which is what a browser and an LLM
        // both send. Coercion happens once the column's type is known.
        let r = query::search(
            &root,
            &q(
                vec![field("severity_number", Op::Gte, QV::Str("9".into()))],
                0,
                i64::MAX,
                100,
            ),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 20);

        // FixedSizeBinary queried as hex.
        let r = query::search(
            &root,
            &q(
                vec![field("trace_id", Op::Eq, QV::Str("07".repeat(16)))],
                0,
                i64::MAX,
                100,
            ),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 20);
        // ...and a malformed one matches nothing instead of a prefix.
        let r = query::search(
            &root,
            &q(
                vec![field("trace_id", Op::Eq, QV::Str("07".into()))],
                0,
                i64::MAX,
                100,
            ),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 0);

        // Terms are AND-ed.
        let r = query::search(
            &root,
            &q(
                vec![attr("service.name", "checkout"), attr("http.method", "GET")],
                0,
                i64::MAX,
                100,
            ),
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 10);

        // THE early exit: with a limit satisfied by the newest block, the older
        // one is never opened even though it overlaps the window.
        let r = query::search(&root, &q(vec![], 0, i64::MAX, 5)).unwrap();
        assert_eq!(r.stats.blocks_scanned, 1);
        assert_eq!(r.json.matches("\"body\"").count(), 5);
        // Newest first. The timestamp is quoted because OTLP/JSON says a 64-bit
        // integer is a string — see [`json::Json::i64_str`].
        assert!(r.json.starts_with("[{\"time_unix_nano\":\"5009\","));
        // Block-local plumbing never reaches the caller.
        assert!(!r.json.contains("resource_id"));
        assert!(!r.json.contains("\"id\""));
        // Scope name is synthesised into scope_attrs at ingest and merges in
        // here, so all three attribute levels are present on one row.
        assert!(r.json.contains("\"otel.scope.name\":\"test\""));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Paging must be a partition of the one-shot answer: every row once, in
    /// the same order, no matter where the page boundaries land.
    ///
    /// The hard part is ties. Four blocks here carry *identical* timestamps, so
    /// every instant is shared by four rows in four different blocks — which is
    /// exactly what a boundary falling mid-nanosecond looks like, and exactly
    /// what a cursor of "the last timestamp I saw" gets wrong. Real ingest makes
    /// these constantly: a batch of records stamped by one call to the clock.
    #[test]
    fn paging_returns_every_row_once_even_across_ties() {
        use query::{Cursor, Search, Signal};

        let root = std::env::temp_dir().join(format!("mira-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Two clusters of two blocks. Within a cluster the timestamps are the
        // same to the nanosecond; the clusters are far apart, so the older one
        // can be pruned by name once paging has passed it.
        for seq in 0..4u64 {
            let base = if seq < 2 { 5_000 } else { 1_000 };
            let mut req = request("svc", 25, base);
            // Five records per nanosecond, so ties exist *inside* a block as
            // well as across them. That matters: within a block rows arrive in
            // ascending row order and the sort key runs descending, so a sort
            // that ignores the tiebreak reorders them, the page boundary lands
            // in the middle of the reordering, and rows come back twice.
            for (i, r) in req.resource_logs[0].scope_logs[0]
                .log_records
                .iter_mut()
                .enumerate()
            {
                r.time_unix_nano = base + i as u64 / 5;
            }
            let mut b = logs::LogsBuilder::new();
            b.append_request(&req).unwrap();
            let sealed = b.finish().unwrap();
            block::publish(&root, "logs", block::node_id("a"), seq, 0, &sealed).unwrap();
        }

        let page = |limit: usize, after| {
            query::search(
                &root,
                &Search {
                    signal: Signal::Logs,
                    from: 0,
                    to: i64::MAX,
                    terms: vec![],
                    limit,
                    after,
                },
            )
            .unwrap()
        };

        let all = page(1_000, None);
        assert_eq!(all.json.matches("\"body\"").count(), 100);
        assert!(all.next.is_none(), "a short page is the last page");

        // 100 rows at 7 a page is 14 full pages and a remainder — so boundaries
        // land inside a tie group on most of them.
        let mut rows = Vec::new();
        let mut after = None;
        let mut deepest = 0;
        for _ in 0..100 {
            let r = page(7, after);
            rows.push(r.json[1..r.json.len() - 1].to_owned());
            deepest = r.stats.blocks_scanned;
            match r.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        assert_eq!(rows.len(), 15);
        assert_eq!(
            format!("[{}]", rows.join(",")),
            all.json,
            "paged reads must reassemble the one-shot answer byte for byte"
        );

        // And the last page is cheaper than the first, not dearer: the two
        // newest blocks are ruled out by name once the cursor is past them.
        // This is the whole reason the cursor is a key and not an offset.
        assert_eq!(all.stats.blocks_scanned, 4);
        assert_eq!(deepest, 2);

        // A cursor from a different query shape is still just a position, and a
        // cursor past the end yields nothing rather than wrapping.
        let end = Cursor {
            ts: 0,
            node: 0,
            seq: 0,
            row: 0,
        };
        assert_eq!(page(7, Some(end)).json, "[]");

        // Round-trips through the wire form, which is the only form a caller
        // ever sees.
        let c = page(7, None).next.unwrap();
        assert_eq!(c.to_string().parse::<Cursor>(), Ok(c));
        for bad in ["", "1.2.3", "1.2.3.4.5", "1.-2.3.4", "a.2.3.4", "1.2.3.x"] {
            assert!(bad.parse::<Cursor>().is_err(), "{bad:?} parsed");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// "Every span of trace X" carries no time bound, so block names prune
    /// nothing and the scan reads the whole signal. The Bloom sidecar is the
    /// only thing standing between that query and every block on disk.
    #[test]
    fn a_trace_lookup_opens_only_the_block_holding_the_trace() {
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span};
        use query::{Op, Search, Signal, Target, Term, Value as QV};

        let root = std::env::temp_dir().join(format!("mira-bloom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Ids from a hash, not a counter: the filter's premise is that trace ids
        // are uniform, and 0,1,2,… would test a distribution that cannot occur.
        let tid = |n: u64| {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&identity::hash64(&n.to_le_bytes()).to_le_bytes());
            b[8..].copy_from_slice(&identity::hash64(&(!n).to_le_bytes()).to_le_bytes());
            b
        };

        const BLOCKS: u64 = 8;
        for seq in 0..BLOCKS {
            let mut b = traces::TracesBuilder::new();
            // Overlapping time ranges on every block, so nothing here can be
            // credited to time pruning.
            let spans = (0..4)
                .map(|i| Span {
                    trace_id: tid(seq * 4 + i).to_vec().into(),
                    span_id: vec![i as u8 + 1; 8].into(),
                    name: "GET /checkout".into(),
                    start_time_unix_nano: 1_000 + i,
                    end_time_unix_nano: 1_100 + i,
                    ..Default::default()
                })
                .collect();
            b.append_request(&ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    resource: Some(Resource::default()),
                    scope_spans: vec![ScopeSpans {
                        spans,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
            .unwrap();
            let sealed = b.finish().unwrap();
            assert!(
                sealed.sidecars.iter().any(|(n, _)| *n == bloom::TRACE_IDX),
                "traces publish a trace filter"
            );
            block::publish(&root, "traces", block::node_id("a"), seq, 0, &sealed).unwrap();
        }

        // The same ids on the logs side. A trace investigation is two questions
        // — the spans, then the logs written under them — and the second one has
        // no more of a time bound than the first, so it needs the same filter.
        for seq in 0..BLOCKS {
            let mut req = request("checkout", 4, 1_000);
            for (i, r) in req.resource_logs[0].scope_logs[0]
                .log_records
                .iter_mut()
                .enumerate()
            {
                r.trace_id = tid(seq * 4 + i as u64).to_vec().into();
            }
            let mut b = logs::LogsBuilder::new();
            b.append_request(&req).unwrap();
            let sealed = b.finish().unwrap();
            block::publish(&root, "logs", block::node_id("a"), seq, 0, &sealed).unwrap();
        }

        let lookup = |signal, id: String| Search {
            signal,
            from: 0,
            to: i64::MAX,
            terms: vec![Term {
                target: Target::Field("trace_id".into()),
                op: Op::Eq,
                value: QV::Str(id),
            }],
            limit: 100,
            after: None,
        };

        let hex = |b: [u8; 16]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        for signal in [Signal::Traces, Signal::Logs] {
            let r = query::search(&root, &lookup(signal, hex(tid(17)))).unwrap();
            assert_eq!(r.stats.blocks_total, BLOCKS as usize);
            assert_eq!(r.stats.blocks_scanned, 1, "{signal:?}: skip the rest");
            assert_eq!(r.stats.rows_matched, 1);

            // An id in no block at all: with 8 filters probed, a false positive
            // is possible, so this asserts the bound rather than zero.
            let r = query::search(&root, &lookup(signal, hex(tid(9_999)))).unwrap();
            assert_eq!(r.stats.rows_matched, 0);
            assert!(r.stats.blocks_scanned <= 1, "{}", r.stats.blocks_scanned);
        }

        // Deleting a sidecar has to cost a block read, never a lost span.
        let one = block::scan(&root, "traces").unwrap();
        for b in &one {
            std::fs::remove_file(b.dir.join(bloom::TRACE_IDX)).unwrap();
        }
        let r = query::search(&root, &lookup(Signal::Traces, hex(tid(17)))).unwrap();
        assert_eq!(r.stats.blocks_scanned, BLOCKS as usize);
        assert_eq!(r.stats.rows_matched, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A scan wide enough to fan out answers exactly as a narrow one would.
    ///
    /// Twenty-four blocks is past the width the waves ramp to, so this runs the
    /// merge at every wave size there is, with several threads reading mappings
    /// at once. What it pins is not speed. Blocks are scanned concurrently and
    /// merged in wave order, so a merge that loses the global ordering — or a
    /// `Hit` whose block index no longer addresses the mapping it came from —
    /// produces a page ordered by wave instead of by timestamp. Every one-block
    /// test in this file passes with that bug in place; paging is what catches
    /// it, because the boundary between two pages is exactly where a
    /// mis-ordering shows up as a row returned twice or not at all.
    ///
    /// The serial half is forced, not hoped for: holding the whole fan-out
    /// budget is the only way to make a wave one block wide on demand, and
    /// without it this would be two runs of whatever the machine felt like
    /// doing. The other half takes the budget back and runs at whatever width
    /// `cargo test`'s own parallelism leaves free, which is the arrangement
    /// every other multi-block test in this file is already under.
    #[test]
    fn a_scan_wide_enough_to_fan_out_answers_exactly_as_a_narrow_one() {
        use query::{Search, Signal};

        let root = std::env::temp_dir().join(format!("mira-fanout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Interleaved in time, not stacked: block 7 holds rows older than block
        // 3's and newer than block 11's, so no wave boundary lines up with a
        // timestamp boundary and the merge has to do real work on every one.
        const BLOCKS: u64 = 24;
        for seq in 0..BLOCKS {
            let base = 1_000_000 + (seq * 7 % BLOCKS) * 1_000;
            let mut req = request("svc", 10, base);
            for (i, r) in req.resource_logs[0].scope_logs[0]
                .log_records
                .iter_mut()
                .enumerate()
            {
                r.time_unix_nano = base + i as u64;
            }
            let mut b = logs::LogsBuilder::new();
            b.append_request(&req).unwrap();
            let sealed = b.finish().unwrap();
            block::publish(&root, "logs", block::node_id("a"), seq, 0, &sealed).unwrap();
        }

        let page = |limit: usize, after| {
            query::search(
                &root,
                &Search {
                    signal: Signal::Logs,
                    from: 0,
                    to: i64::MAX,
                    terms: vec![],
                    limit,
                    after,
                },
            )
            .unwrap()
        };

        // Serial: the whole fan-out budget is held elsewhere, so every wave is
        // one block wide and the loop is the one this replaced.
        let held = query::Helpers::claim(usize::MAX);
        let serial = page(1_000, None);
        drop(held);

        // No `limit` to exit on, so every block is read and every wave runs.
        let all = page(1_000, None);
        assert_eq!(all.stats.blocks_scanned, BLOCKS as usize);
        assert_eq!(all.json.matches("\"body\"").count(), 240);
        assert_eq!(all.json, serial.json, "fanning out changed the answer");

        let mut rows = Vec::new();
        let mut after = None;
        for _ in 0..100 {
            let r = page(7, after);
            rows.push(r.json[1..r.json.len() - 1].to_owned());
            match r.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        assert_eq!(
            format!("[{}]", rows.join(",")),
            all.json,
            "a fanned-out scan must page like a serial one"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A three-service call chain, walked with the frame algebra (section 7.3).
    ///
    /// One fixture for the whole of `frame.rs`, because the interesting claims
    /// are all about how its pieces relate and each of them needs the same
    /// store. In order:
    ///
    /// - `anchor` gives back the trace and the *one* service of the row that
    ///   matched, not everything in the window.
    /// - The expanders do not commute. `peers` alone finds nothing, because the
    ///   log that anchored the frame was written seconds after the request it
    ///   describes and the trace blocks do not overlap that window at all;
    ///   `traces` first measures the real extent and then `peers` finds all
    ///   three. This is the bug the algebra exists to make unwriteable, and it
    ///   is silent — the wrong order returns a confident, short answer.
    /// - `map` reconstructs the topology from `parent_span_id` alone, including
    ///   the entry edge for the root span and the error the leaf reported.
    #[test]
    fn a_frame_walk_reconstructs_a_call_chain_from_one_log_line() {
        use mira_proto::collector::trace::v1::ExportTraceServiceRequest;
        use mira_proto::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
        use query::{Op, Search, Signal, Target, Term, Value as QV};

        let root = std::env::temp_dir().join(format!("mira-frame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Spans start here; the logs are written five seconds later, which is
        // what makes the widening observable rather than incidental.
        const T0: u64 = 1_000_000_000;
        const LOG_TS: u64 = T0 + 5_000_000_000;
        let tid = |n: u8| [n; 16];
        let sid = |svc: u8, n: u8| [svc, n, 0, 0, 0, 0, 0, 0];

        // gateway -> api -> db, three traces, the third failing at the leaf.
        let svc = |name: &str, spans: Vec<Span>| ResourceSpans {
            resource: Some(Resource {
                // The same pair `request` writes on the logs side, so the two
                // signals land on the same `resources.key` and the join below
                // is a real cross-signal join rather than two disjoint sets
                // that happen to be counted together.
                attributes: vec![kv("service.name", name), kv("service.instance.id", "7f3a")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        };
        let span = |t: u8, id: [u8; 8], parent: Option<[u8; 8]>, dur: u64, bad: bool| Span {
            trace_id: tid(t).to_vec().into(),
            span_id: id.to_vec().into(),
            parent_span_id: parent.map(|p| p.to_vec()).unwrap_or_default().into(),
            name: "GET /checkout".into(),
            start_time_unix_nano: T0 + u64::from(t),
            end_time_unix_nano: T0 + u64::from(t) + dur,
            status: bad.then(|| Status {
                code: 2,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut b = traces::TracesBuilder::new();
        b.append_request(&ExportTraceServiceRequest {
            resource_spans: vec![
                svc(
                    "gateway",
                    (1..=3)
                        .map(|t| span(t, sid(1, t), None, 900, false))
                        .collect(),
                ),
                svc(
                    "api",
                    (1..=3)
                        .map(|t| span(t, sid(2, t), Some(sid(1, t)), 500, false))
                        .collect(),
                ),
                svc(
                    "db",
                    (1..=3)
                        .map(|t| span(t, sid(3, t), Some(sid(2, t)), 100, t == 3))
                        .collect(),
                ),
            ],
        })
        .unwrap();
        let sealed = b.finish().unwrap();
        block::publish(&root, "traces", block::node_id("a"), 0, 0, &sealed).unwrap();

        // One log line per trace, from `api`, long after the request finished —
        // the shape of an async worker logging what it did.
        let mut req = request("api", 3, LOG_TS);
        for (i, r) in req.resource_logs[0].scope_logs[0]
            .log_records
            .iter_mut()
            .enumerate()
        {
            r.trace_id = tid(i as u8 + 1).to_vec().into();
            r.body = Some(AnyValue {
                value: Some(Value::StringValue(if i == 2 {
                    "boom".into()
                } else {
                    "fine".into()
                })),
            });
        }
        let mut b = logs::LogsBuilder::new();
        b.append_request(&req).unwrap();
        let sealed = b.finish().unwrap();
        block::publish(&root, "logs", block::node_id("a"), 0, 0, &sealed).unwrap();

        // "The error I am looking at", as a search a UI would send.
        let q = Search {
            signal: Signal::Logs,
            from: LOG_TS as i64 - 1_000_000_000,
            to: LOG_TS as i64 + 1_000_000_000,
            terms: vec![Term {
                target: Target::Field("body".into()),
                op: Op::Eq,
                value: QV::Str("boom".into()),
            }],
            limit: 10,
            after: None,
        };
        let (f, st) = frame::anchor(&root, &q, &[]).unwrap();
        assert_eq!(f.traces, vec![tid(3)], "one row matched, so one trace");
        assert_eq!(f.entities.len(), 1, "the service that wrote the line, only");
        let api = f.entities[0];
        assert!(!f.truncated);
        assert_eq!(st.rows_matched, 1);

        // Wrong order: the trace blocks end five seconds before this window
        // opens, so there is nothing to join against.
        let (early, _) = frame::expand(&root, &f, &[frame::Expand::Peers], &[]).unwrap();
        assert_eq!(early.entities, f.entities, "peers before traces finds none");

        let ops = [frame::Expand::Traces, frame::Expand::Peers];
        let (walked, st) = frame::expand(&root, &f, &ops, &[]).unwrap();
        assert_eq!(
            walked.from,
            T0 as i64 + 3,
            "the window reaches back to the first span of the trace"
        );
        assert_eq!(walked.to, q.to, "the traces end inside the window already");
        assert_eq!(walked.entities.len(), 3, "gateway, api and db");
        // Six span rows read for three matches, twice — once per expander.
        assert_eq!((st.rows_scanned, st.rows_matched), (18, 6));

        let names = frame::names_of(&root, &walked, &[]).unwrap();
        assert_eq!(
            names.get(&api).map(String::as_str),
            Some("api"),
            "the key harvested from a log row is the key the spans carry"
        );
        let mut got: Vec<&str> = names.values().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, ["api", "db", "gateway"]);

        // The topology, from `parent_span_id` and nothing else.
        let m = frame::map(&root, 0, i64::MAX, 1_000_000, &[]).unwrap();
        let edge = |from: &str, to: &str| {
            let (fk, tk) = (key_of(&names, from), key_of(&names, to));
            let from = if from == "entry" {
                "\"entry\"".to_owned()
            } else {
                format!("\"{fk}\"")
            };
            format!("\"from\":{from},\"to\":\"{tk}\"")
        };
        for (a, b) in [("entry", "gateway"), ("gateway", "api"), ("api", "db")] {
            assert!(m.json.contains(&edge(a, b)), "{a} -> {b} in {}", m.json);
        }
        assert!(
            m.json.contains("\"unresolved\":0"),
            "every parent is in the same block: {}",
            m.json
        );
        assert_eq!(
            m.json.matches("\"errors\":1").count(),
            2,
            "the failing leaf shows on its node and on its one in-edge: {}",
            m.json
        );

        // The facet a service picker is built from.
        let e = frame::entities(&root, 0, i64::MAX, &[]).unwrap();
        assert_eq!(e.stats.rows_matched, 3);
        assert!(
            e.json.find("gateway") > e.json.find("db"),
            "sorted by name, for a human: {}",
            e.json
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `resources.key` a name maps back to, for asserting on rendered ids.
    fn key_of(names: &std::collections::HashMap<u64, String>, name: &str) -> u64 {
        names
            .iter()
            .find(|(_, v)| *v == name)
            .map_or(0, |(k, _)| *k)
    }

    /// The attribute filter has one dangerous property, and every case here is
    /// an instance of it: a query scalar is compared against whatever type the
    /// SDK happened to store, so `{eq: "200"}` finds an integer `200` and
    /// `{eq: true}` finds a boolean. A filter that indexed the typed bytes would
    /// disagree with that rule and prune the block holding the row — and a
    /// pruned block is not a slow query, it is a row that silently does not
    /// exist. Indexing the value's *text* is what keeps the two in step.
    #[test]
    fn the_attribute_filter_skips_blocks_without_losing_coercions() {
        use query::{Op, Search, Signal, Target, Term, Value as QV};

        let root = std::env::temp_dir().join(format!("mira-attrs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let attr = |k: &str, v: Value| KeyValue {
            key: k.into(),
            value: Some(AnyValue { value: Some(v) }),
        };

        const BLOCKS: u64 = 8;
        for seq in 0..BLOCKS {
            // Overlapping time ranges, so nothing below can be credited to the
            // block name.
            let mut req = request("checkout", 2, 1_000);
            let mut attrs = vec![attr(
                "k8s.pod.name",
                Value::StringValue(format!("api-{seq}")),
            )];
            // One block carries the three non-string types.
            if seq == 3 {
                attrs.push(attr("http.status_code", Value::IntValue(200)));
                attrs.push(attr("retry", Value::BoolValue(true)));
                attrs.push(attr("ratio", Value::DoubleValue(1.5)));
                // The same kind of number, stored as text — which is what a
                // good half of the SDKs emitting a status code actually send.
                attrs.push(attr("status.text", Value::StringValue("404".into())));
            }
            for r in &mut req.resource_logs[0].scope_logs[0].log_records {
                r.attributes = attrs.clone();
            }
            let mut b = logs::LogsBuilder::new();
            b.append_request(&req).unwrap();
            let sealed = b.finish().unwrap();
            let names: Vec<_> = sealed.sidecars.iter().map(|(n, _)| *n).collect();
            assert_eq!(
                names,
                [bloom::ATTR_IDX, zone::ZONE_IDX, bloom::TRACE_IDX],
                "logs sidecars"
            );
            block::publish(&root, "logs", block::node_id("a"), seq, 0, &sealed).unwrap();
        }

        let find_op = |key: &str, op: Op, value: QV| {
            query::search(
                &root,
                &Search {
                    signal: Signal::Logs,
                    from: 0,
                    to: i64::MAX,
                    terms: vec![Term {
                        target: Target::Attr(key.into()),
                        op,
                        value,
                    }],
                    limit: 100,
                    after: None,
                },
            )
            .unwrap()
        };
        let find = |key: &str, value: QV| find_op(key, Op::Eq, value);
        let s = |v: &str| QV::Str(v.into());

        // The plain case: one block holds the value, seven are ruled out
        // without being opened.
        let r = find("k8s.pod.name", s("api-5"));
        assert_eq!(r.stats.blocks_total, BLOCKS as usize);
        assert_eq!(r.stats.blocks_scanned, 1, "the filter must skip the rest");
        assert_eq!(r.stats.rows_matched, 2);

        // A value in no block at all. This is the query the filter exists for —
        // it has no early exit, so unfiltered it reads all of retention to prove
        // a negative. One false positive is allowed for; eight would mean the
        // filter is not working.
        let r = find("k8s.pod.name", s("api-99"));
        assert_eq!(r.stats.rows_matched, 0);
        assert!(r.stats.blocks_scanned <= 1, "{}", r.stats.blocks_scanned);

        // The coercions. Each of these must find the two rows in block 3.
        for (key, value) in [
            ("http.status_code", QV::Int(200)),
            ("http.status_code", s("200")),
            // A whole double reaches an integer column through `as_i64`.
            ("http.status_code", QV::Double(200.0)),
            ("retry", QV::Bool(true)),
            ("retry", s("true")),
            // Doubles are not indexed at all; block 3 declares it holds some and
            // is scanned, and the other seven are still skipped.
            ("ratio", QV::Double(1.5)),
            ("ratio", s("1.5")),
        ] {
            let r = find(key, value.clone());
            assert_eq!(r.stats.rows_matched, 2, "{key} = {value:?}");
        }

        // The reverse direction, which used to be the asymmetry: every other
        // arm parses a string, so `eq: "200"` finds an integer column, but the
        // string column read only `Value::Str` and `eq: 404` against text was
        // unconditionally false. The index never agreed — it holds the text
        // "404" either way and pointed straight at the block.
        let r = find("status.text", QV::Int(404));
        assert_eq!(r.stats.rows_matched, 2);
        assert_eq!(r.stats.blocks_scanned, 1, "and it is still pruned");

        // Ordering against a number stored as text is numeric, not
        // lexicographic. "404" is below 500 both ways, but above 99 only one of
        // them — a byte comparison puts '4' before '9' and answers no.
        //
        // These are also the queries the zone map exists for: an ordering has no
        // value to hash, so the attribute filter cannot rule out a single block
        // and all eight used to be opened to find the one. The map has to agree
        // with the numeric reading above — it folds the parseable text into its
        // double range for exactly that reason.
        let r = find_op("status.text", Op::Lt, QV::Int(500));
        assert_eq!(r.stats.rows_matched, 2);
        assert_eq!(r.stats.blocks_scanned, 1, "the zone map must skip the rest");
        let r = find_op("status.text", Op::Gt, QV::Int(99));
        assert_eq!(r.stats.rows_matched, 2);
        assert_eq!(r.stats.blocks_scanned, 1);
        // An ordering no block can reach opens nothing at all. Block 3's only
        // status code is 200; the other seven have no such key, and a key the
        // map does not hold is a key with no comparable value.
        let r = find_op("http.status_code", Op::Gte, QV::Int(500));
        assert_eq!(r.stats.rows_matched, 0);
        assert_eq!(r.stats.blocks_scanned, 0);
        // A quoted scalar asked for a text comparison and still gets one.
        assert_eq!(
            find_op("status.text", Op::Gt, s("99")).stats.rows_matched,
            0
        );
        // `contains` renders the scalar rather than refusing it.
        assert_eq!(
            find_op("status.text", Op::Contains, QV::Int(40))
                .stats
                .rows_matched,
            2
        );

        // A string that cannot be read as a number must still prune the block
        // that holds doubles — otherwise `HAS_DOUBLE` would disable the filter
        // for every text query in a block with one float in it.
        let r = find("ratio", s("banana"));
        assert_eq!(r.stats.rows_matched, 0);
        assert!(r.stats.blocks_scanned <= 1, "{}", r.stats.blocks_scanned);

        // Deleting a sidecar has to cost a block read, never a lost row. Both
        // of them: a block published before either index existed still answers
        // every query, slowly.
        for b in block::scan(&root, "logs").unwrap() {
            std::fs::remove_file(b.dir.join(bloom::ATTR_IDX)).unwrap();
            std::fs::remove_file(b.dir.join(zone::ZONE_IDX)).unwrap();
        }
        let r = find("k8s.pod.name", s("api-5"));
        assert_eq!(r.stats.blocks_scanned, BLOCKS as usize);
        assert_eq!(r.stats.rows_matched, 2);
        let r = find_op("status.text", Op::Lt, QV::Int(500));
        assert_eq!(r.stats.blocks_scanned, BLOCKS as usize);
        assert_eq!(r.stats.rows_matched, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The headroom hint is a conservative guess, and a big export makes it
    /// guess wrong. It has to stay wrong in that direction — counting distinct
    /// keys would mean hashing every request on the hot path — so what matters
    /// is that the guess is never mistaken for the real ceiling.
    ///
    /// `pipeline::flusher` relies on exactly this: on an empty block it skips
    /// the hint and appends, because the hint saying no is not evidence that the
    /// data does not fit.
    #[test]
    fn a_large_export_the_headroom_hint_rejects_still_fits_one_block() {
        // 20k records x 4 attributes = 80,000 attribute rows, past DICT_CAP,
        // from a grand total of five distinct keys.
        let mut req = request("checkout", 20_000, 1_000);
        for r in &mut req.resource_logs[0].scope_logs[0].log_records {
            r.attributes = vec![
                kv("http.method", "GET"),
                kv("http.route", "/checkout"),
                kv("net.peer.name", "db"),
                kv("http.scheme", "https"),
            ];
        }

        let mut b = logs::LogsBuilder::new();
        assert!(
            !b.has_headroom_for(&req),
            "the hint is expected to be conservative here; if it stopped being \
             so, this test is no longer testing anything"
        );
        // ...and yet.
        assert_eq!(b.append_request(&req).unwrap(), 20_000);
        let sealed = b.finish().unwrap();
        assert_eq!(sealed.num_rows, 20_000);
        assert_eq!(sealed.table("log_attrs").unwrap().num_rows(), 80_000);
    }

    /// The cold tier: an aged block is rewritten compressed in place, and every
    /// read path keeps working over a directory that is mid-rewrite.
    #[test]
    fn compaction_shrinks_aged_blocks_without_changing_what_they_answer() {
        use query::{Op, Search, Signal, Target, Term, Value as QV};

        let root = std::env::temp_dir().join(format!("mira-cold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // One block an hour and a half old, one from a minute ago.
        let now = 4 * block::COLD_AFTER_NS;
        let old = (now - 90 * 60 * 1_000_000_000) as u64;
        let new = (now - 60 * 1_000_000_000) as u64;
        let mut dirs = Vec::new();
        for (seq, base) in [old, new].into_iter().enumerate() {
            let mut b = logs::LogsBuilder::new();
            b.append_request(&request("checkout", 500, base)).unwrap();
            let sealed = b.finish().unwrap();
            let node = block::node_id("a");
            dirs.push(
                block::publish(&root, "logs", node, seq as u64, 0, &sealed)
                    .unwrap()
                    .dir,
            );
        }
        let size = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "arrow"))
                .map(|e| e.metadata().unwrap().len())
                .sum::<u64>()
        };
        let before = size(&dirs[0]);

        // A crash between two tables leaves a directory holding both tiers. The
        // codec is per-batch IPC metadata, so that has to read, and the next
        // sweep has to finish rather than skip it.
        let partial = dirs[0].join("log_attrs.arrow");
        let staged = dirs[0].join("log_attrs.staged");
        let batch = block::open_table(&partial).unwrap().batches[0].clone();
        // Staged then renamed, not written over the live name: `batch` still
        // points into the mapping of `partial`, and truncating a mapped file is
        // a SIGBUS on the next page touched. This is the constraint `compact`
        // is built around, not a detail of the test.
        block::write_table_zstd(&staged, &batch).unwrap();
        drop(batch);
        std::fs::rename(&staged, &partial).unwrap();
        assert!(block::open_table(&dirs[0].join("logs.arrow")).is_ok());

        let n = block::compact(
            &root,
            "logs",
            block::node_id("a"),
            now - block::COLD_AFTER_NS,
        )
        .unwrap();
        assert_eq!(n, 1, "only the aged block is cold");
        assert!(dirs[0].join("cold").exists());
        assert!(!dirs[1].join("cold").exists(), "the fresh block is hot");
        let after = size(&dirs[0]);
        assert!(after * 2 < before, "{before} -> {after} is not a tier");

        // Idempotent: the marker means the second sweep does no IO at all.
        let stamp = std::fs::metadata(dirs[0].join("logs.arrow"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            block::compact(
                &root,
                "logs",
                block::node_id("a"),
                now - block::COLD_AFTER_NS
            )
            .unwrap(),
            0
        );
        assert_eq!(
            std::fs::metadata(dirs[0].join("logs.arrow"))
                .unwrap()
                .modified()
                .unwrap(),
            stamp
        );

        // The trade, stated: the cold block gives up zero-copy, the hot one does
        // not. Nothing else about either read changes.
        let cold = block::open_table(&dirs[0].join("logs.arrow")).unwrap();
        let (inside, total) = cold.zero_copy_ratio();
        assert_eq!(cold.batches[0].num_rows(), 500);
        // Not zero: arrow leaves an empty buffer — an all-valid null mask — as a
        // zero-length slice of the mapping rather than allocating nothing.
        assert!(
            inside < total,
            "{inside}/{total} — a decompressed buffer is a copy"
        );
        let hot = block::open_table(&dirs[1].join("logs.arrow")).unwrap();
        let (inside, total) = hot.zero_copy_ratio();
        assert_eq!(inside, total, "the hot tier stays zero-copy");

        // And the answer is the same one the plain block gave: both blocks, both
        // attribute levels, through the sidecars that compaction left alone.
        let r = query::search(
            &root,
            &Search {
                signal: Signal::Logs,
                from: 0,
                to: i64::MAX,
                terms: vec![Term {
                    target: Target::Attr("service.name".into()),
                    op: Op::Eq,
                    value: QV::Str("checkout".into()),
                }],
                limit: 2_000,
                after: None,
            },
        )
        .unwrap();
        assert_eq!(r.stats.rows_matched, 1_000);
        assert_eq!(r.stats.blocks_scanned, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The startup guard has to say yes to an ordinary local directory. It
    /// cannot be tested against a real NFS mount here, so this is the half that
    /// catches the failure that would actually happen: a guard that refuses
    /// everything, or one that errors on a path it should ignore.
    #[test]
    fn the_filesystem_guard_passes_a_local_directory() {
        let dir = std::env::temp_dir().join(format!("mira-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        block::check_filesystem(&dir).unwrap();
        // A path that does not exist is not a filesystem verdict.
        block::check_filesystem(&dir.join("nope")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other startup guard. `create_dir_all` says yes to a directory that
    /// already exists whatever its mode, so without this a read-only data
    /// directory reaches a listening socket and fails one export at a time.
    #[test]
    fn the_write_guard_refuses_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mira-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        block::check_writable(&dir).unwrap();
        // And it left nothing behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root defeats the mode bits entirely, so ask the filesystem rather
        // than assume: where a bare write still succeeds, the guard passing is
        // the right answer and there is nothing here to assert.
        let blocked = std::fs::write(dir.join("canary"), []).is_err();
        let verdict = block::check_writable(&dir);
        assert_eq!(
            verdict.is_err(),
            blocked,
            "the guard must agree with the filesystem: {verdict:?}"
        );
        assert!(
            !blocked || matches!(verdict, Err(Error::NotWritable { .. })),
            "{verdict:?}"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // A directory that is not there at all is a different error, but still
        // an error rather than a successful start.
        assert!(block::check_writable(&dir.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_body_is_caught_not_returned_as_data() {
        let dir = std::env::temp_dir().join(format!("mira-crc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logs.arrow");

        let mut b = logs::LogsBuilder::new();
        b.append_request(&request("checkout", 64, 1_000)).unwrap();
        let sealed = b.finish().unwrap();
        block::write_table(&path, sealed.table("logs").unwrap()).unwrap();
        assert!(block::open_table(&path).is_ok());

        // Flip one bit deep in the body. Arrow's own reader would decode this
        // into wrong answers without complaint.
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(
            block::open_table(&path),
            Err(Error::BadChecksum { .. })
        ));

        // Truncated from the front, and not an Arrow file at all. arrow-rs seeks
        // straight to the trailer and never looks at the magic, so both of these
        // reach the decoder as garbage unless we check first.
        for junk in [&b"ARROW"[..], &[0u8; 64][..]] {
            std::fs::write(&path, junk).unwrap();
            assert!(
                matches!(block::open_table(&path), Err(Error::BadMagic { .. })),
                "{junk:?} decoded as a block"
            );
        }

        // A real Arrow file, written by arrow-rs rather than by us, so the magic
        // and the footer are both valid and only the checksum is absent. Reading
        // it would be reading something no version of Mira wrote.
        let batch = sealed.table("logs").unwrap();
        let mut w = arrow_ipc::writer::FileWriter::try_new(
            std::fs::File::create(&path).unwrap(),
            &batch.schema(),
        )
        .unwrap();
        w.write(batch).unwrap();
        w.finish().unwrap();
        // `.err().expect` rather than `expect_err`: a `MappedTable` is an mmap
        // and a schema, and deriving `Debug` on it to print one in a test that
        // must never see one is the wrong direction.
        let e = block::open_table(&path)
            .err()
            .expect("a file with no checksum in it was read as a block");
        assert!(matches!(e, Error::MissingMetadata { .. }), "{e}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The block directory is the manifest, which means everything else on the
    /// volume is noise it has to survive: a `.DS_Store`, a directory somebody
    /// else made, a block half-published by a process that was killed.
    ///
    /// None of these are hypothetical. Replicas share a volume by design (section 10),
    /// and the catalog is rebuilt by `readdir` on every start.
    #[test]
    fn the_catalog_ignores_everything_it_did_not_write() {
        let root = std::env::temp_dir().join(format!("mira-junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let node = block::node_id("a");

        let seal = |base: u64| {
            let mut b = logs::LogsBuilder::new();
            b.append_request(&request("checkout", 4, base)).unwrap();
            b.finish().unwrap()
        };
        let published = block::publish(&root, "logs", node, 0, 0, &seal(1_000)).unwrap();
        let partition = published.dir.parent().unwrap().to_path_buf();
        let name = published.dir.file_name().unwrap().to_str().unwrap();

        // A file where a partition directory should be, a directory whose name
        // is not a block name, and a block name with one field too many — which
        // is what a future format version would look like from here.
        std::fs::write(root.join("logs").join(".DS_Store"), b"junk").unwrap();
        for junk in ["not-a-block", &format!("{name}-1")] {
            std::fs::create_dir_all(partition.join(junk)).unwrap();
        }
        assert_eq!(block::scan(&root, "logs").unwrap(), vec![published.clone()]);

        // A publish killed between staging and rename leaves the staging
        // directory behind. The name is unique per block, so nothing reuses it
        // and it is `sweep_staging` at boot that clears it — filtered by signal
        // and node, so a directory belonging to another replica sharing the
        // volume, or to another signal, survives untouched.
        let stale = root
            .join(".tmp")
            .join(format!("logs-{node:08x}-{:012}-x", 1));
        let other_node = root.join(".tmp").join("logs-deadbeef-000000000001-x");
        let other_signal = root.join(".tmp").join(format!("traces-{node:08x}-1-x"));
        for d in [&stale, &other_node, &other_signal] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("logs.arrow"), b"leftovers").unwrap();
        }
        assert_eq!(block::sweep_staging(&root, "logs", node).unwrap(), 1);
        assert!(!stale.exists());
        assert!(other_node.is_dir() && other_signal.is_dir());
        // Sweeping a data directory that never staged anything is not an error.
        assert_eq!(
            block::sweep_staging(&root.join("nope"), "logs", node).unwrap(),
            0
        );

        let second = block::publish(&root, "logs", node, 1, 0, &seal(2_000)).unwrap();
        assert!(block::open_table(&second.dir.join("logs.arrow")).is_ok());
        assert_eq!(block::scan(&root, "logs").unwrap().len(), 2);

        // And retention takes the block, not the junk beside it.
        assert_eq!(block::expire(&root, "logs", i64::MAX).unwrap(), 2);
        assert!(block::scan(&root, "logs").unwrap().is_empty());
        assert!(partition.join("not-a-block").is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every codec writes a file the reader reads back identically. LZ4 is not
    /// written by the engine — it exists so the `tier` example can price the
    /// pure-Rust codec against the C one — and an untested writer is how that
    /// comparison ends up measuring a bug.
    #[test]
    fn every_codec_round_trips_the_same_rows() {
        let dir = std::env::temp_dir().join(format!("mira-codec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut b = logs::LogsBuilder::new();
        b.append_request(&request("checkout", 500, 1_000)).unwrap();
        let sealed = b.finish().unwrap();
        let batch = sealed.table("logs").unwrap();

        type Writer = fn(&std::path::Path, &arrow_array::RecordBatch) -> Result<()>;
        let write: [(&str, Writer); 3] = [
            ("plain", block::write_table),
            ("zstd", block::write_table_zstd),
            ("lz4", block::write_table_lz4),
        ];
        let mut sizes = Vec::new();
        for (name, f) in write {
            let path = dir.join(format!("{name}.arrow"));
            f(&path, batch).unwrap();
            let read = block::open_table(&path).unwrap();
            assert_eq!(read.batches, vec![batch.clone()], "{name}");
            sizes.push((name, std::fs::metadata(&path).unwrap().len()));
        }
        let plain = sizes[0].1;
        for (name, size) in &sizes[1..] {
            assert!(*size < plain, "{name} is {size} against {plain} plain");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
