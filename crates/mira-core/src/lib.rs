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
//! * [`logs`] / [`traces`] — OTLP protobuf into those schemas, with block-local
//!   id rebasing.
//! * [`block`] — atomic publish of immutable block directories and zero-copy
//!   mmap reads back out of them.
//!
//! See `docs/ARCHITECTURE.md` for why each of those is shaped the way it is.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod attrs;
pub mod block;
pub mod error;
pub mod identity;
pub mod logs;
pub mod schema;
pub mod signal;
pub mod traces;

pub use error::{Error, Result};
pub use signal::{Sealed, SignalBuilder};

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
        let published = block::publish(
            &root,
            "logs",
            node,
            1,
            sealed.min_ts,
            sealed.max_ts,
            &sealed.refs(),
        )
        .unwrap();
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
        let published = block::publish(
            &root,
            "traces",
            block::node_id("a"),
            1,
            sealed.min_ts,
            sealed.max_ts,
            &sealed.refs(),
        )
        .unwrap();
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

        let _ = std::fs::remove_dir_all(&dir);
    }
}
