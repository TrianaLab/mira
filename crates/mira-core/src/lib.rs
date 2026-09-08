//! Mira storage engine.
//!
//! Three layers, in dependency order:
//!
//! * [`schema`] — the OTAP-shaped Arrow schemas that are simultaneously the
//!   in-memory and the on-disk layout. There is no translation step.
//! * [`identity`] — the stable entity key that correlation joins on.
//! * [`signal`] — the one shape every signal's encoder presents to the flusher.
//! * [`logs`] — OTLP protobuf into those schemas, with block-local id rebasing.
//! * [`block`] — atomic publish of immutable block directories and zero-copy
//!   mmap reads back out of them.
//!
//! See `docs/ARCHITECTURE.md` for why each of those is shaped the way it is.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod block;
pub mod error;
pub mod identity;
pub mod logs;
pub mod schema;
pub mod signal;

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
        wide.resource_logs[0].scope_logs[0].log_records[0].attributes = (0..=logs::DICT_CAP)
            .map(|i| kv(&format!("k{i}"), "v"))
            .collect();
        assert!(!small.has_headroom_for(&wide));
        assert!(small.has_headroom_for(&request("checkout", 10, 1)));
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
