//! Mira storing Mira's telemetry, in Mira.
//!
//! Every counter here already existed and is already served, in one shape, by
//! `/api/v1/stats`. What that endpoint cannot do is answer "when did the shed
//! rate start climbing", because it is an instant and an instant has no
//! yesterday. The gap between a node that knows everything about itself right
//! now and a node that remembers is a time series database, and this process is
//! one — so the whole feature is a timer, a translation into OTLP, and a call
//! to the same [`pipeline::Ingest::submit`] a collector would have used.
//!
//! No exporter, no scrape endpoint, no second port. That is the point rather
//! than a shortcut: the reason self-monitoring is normally somebody else's
//! Prometheus is that the thing being monitored cannot be trusted to store its
//! own data, and the reason it can be trusted here is that when Mira is too
//! broken to store this, the operator's evidence is `/health` and the process
//! exit code, which do not depend on it. A node that cannot write its own
//! metrics is a node whose missing metrics *are* the signal.
//!
//! It is off by default. See [`crate::config::Config::self_telemetry`] for why that is the
//! honest default rather than a timid one.
//!
//! ## The reflexive bit
//!
//! Storing these rows increments `mira.ingest.rows`, which is reported in the
//! next sample. That is not a bug to be corrected out: the sampler's own cost is
//! part of the node's cost, and subtracting it would make the number disagree
//! with the block directory. At the default interval it is three-figures of rows
//! an hour against a floor of millions, so it is visible in arithmetic and
//! invisible on a chart.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use mira_proto::collector::metrics::v1::ExportMetricsServiceRequest;
use mira_proto::common::v1::{AnyValue, KeyValue, any_value};
use mira_proto::metrics::v1::{
    AggregationTemporality, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
    metric, number_data_point,
};
use mira_proto::resource::v1::Resource;

use crate::pipeline;

/// The instrumentation scope every series below is published under, so a reader
/// can separate what Mira says about itself from what was sent to it by a
/// service that happens to also be called `mira`.
const SCOPE: &str = "mira.self";

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
    }
}

/// One cumulative counter: a value that only goes up for the life of the
/// process, which is what `is_monotonic` promises a reader.
fn sum(name: &str, unit: &str, description: &str, points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: unit.into(),
        description: description.into(),
        data: Some(metric::Data::Sum(Sum {
            data_points: points,
            aggregation_temporality: AggregationTemporality::Cumulative as i32,
            is_monotonic: true,
        })),
        ..Default::default()
    }
}

/// One instantaneous reading. Not a `Sum` with `is_monotonic: false`, which is
/// the other legal spelling: a gauge is what every consumer's default renderer
/// expects for "the value right now", and being unusual here buys nothing.
fn gauge(name: &str, unit: &str, description: &str, points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: unit.into(),
        description: description.into(),
        data: Some(metric::Data::Gauge(Gauge {
            data_points: points,
        })),
        ..Default::default()
    }
}

/// Everything this node currently knows about itself, as OTLP.
///
/// `start` is the process start in Unix nanoseconds and goes on every point:
/// without it a cumulative counter has no way to say "this stream began here",
/// and a consumer computing a rate across a restart reads the reset as a
/// enormous negative delta. `blocks` is `None` for a signal whose directory
/// would not list, and a point that cannot be measured is omitted rather than
/// sent as zero — the same rule `/api/v1/stats` follows with `null`.
pub fn sample(
    node: &str,
    now: u64,
    start: u64,
    uptime_s: u64,
    peak_rss: u64,
    free_fraction: Option<f64>,
    blocks: [Option<u64>; pipeline::SIGNALS.len()],
) -> ExportMetricsServiceRequest {
    let point = |v: f64, attrs: Vec<KeyValue>| NumberDataPoint {
        attributes: attrs,
        start_time_unix_nano: start,
        time_unix_nano: now,
        value: Some(number_data_point::Value::AsDouble(v)),
        ..Default::default()
    };
    let queries = crate::QUERIES.load(Relaxed);

    let mut metrics = vec![
        gauge(
            "mira.uptime",
            "s",
            "Seconds since this process started serving",
            vec![point(uptime_s as f64, Vec::new())],
        ),
        gauge(
            "mira.process.memory.peak",
            "By",
            "Peak resident set, page cache included",
            vec![point(peak_rss as f64, Vec::new())],
        ),
        sum(
            "mira.query.count",
            "1",
            "Reads answered since start",
            vec![point(queries as f64, Vec::new())],
        ),
        gauge(
            "mira.query.duration.max",
            "ms",
            "Slowest read since start, queue time included",
            vec![point(
                crate::QUERY_MAX_NANOS.load(Relaxed) as f64 / 1e6,
                Vec::new(),
            )],
        ),
        // Mean rather than a histogram because the source is a running total and
        // a count, and inventing buckets from those two numbers would be
        // inventing the distribution. The max above is what catches the tail.
        gauge(
            "mira.query.duration.mean",
            "ms",
            "Mean read latency since start",
            vec![point(
                crate::QUERY_NANOS.load(Relaxed) as f64 / queries.max(1) as f64 / 1e6,
                Vec::new(),
            )],
        ),
    ];
    if let Some(f) = free_fraction {
        metrics.push(gauge(
            "mira.storage.free",
            "1",
            "Free fraction of the filesystem holding the block directory",
            vec![point(f, Vec::new())],
        ));
    }

    // One series per counter with the signal as an attribute, rather than one
    // metric per signal: `sum(mira.ingest.rows)` is then the node's total and
    // `by signal` is the breakdown, which is the shape every query language
    // already knows how to ask for.
    let by_signal = |f: &dyn Fn(&pipeline::Rejects) -> u64| {
        pipeline::REJECTS
            .iter()
            .map(|r| point(f(r) as f64, vec![kv("signal", r.signal)]))
            .collect::<Vec<_>>()
    };
    metrics.extend([
        sum(
            "mira.ingest.rows",
            "1",
            "Records written to blocks",
            by_signal(&|r| r.rows.load(Relaxed)),
        ),
        sum(
            "mira.ingest.bytes",
            "By",
            "Bytes those records took on disk",
            by_signal(&|r| r.bytes.load(Relaxed)),
        ),
        sum(
            "mira.ingest.blocks",
            "1",
            "Blocks published",
            by_signal(&|r| r.published.load(Relaxed)),
        ),
        sum(
            "mira.ingest.shed",
            "1",
            "Exports refused with a 503 because the queue was full",
            by_signal(&|r| r.shed.load(Relaxed)),
        ),
        sum(
            "mira.ingest.failed",
            "1",
            "Exports accepted and then NACKed because the write did not land",
            by_signal(&|r| r.failed.load(Relaxed)),
        ),
        sum(
            "mira.ingest.refused",
            "1",
            "Exports refused permanently: the only counter that measures lost data",
            by_signal(&|r| r.refused.load(Relaxed)),
        ),
        // Age, not the timestamp: a chart of "seconds the open block has been
        // open" has a ceiling an operator can reason about (`max_block_age`),
        // and a chart of Unix seconds is a diagonal line.
        gauge(
            "mira.ingest.open_block.age",
            "s",
            "How long the currently open block has been open, 0 if none is",
            by_signal(&|r| match r.open_since.load(Relaxed) {
                0 => 0,
                since => now / 1_000_000_000 - since.min(now / 1_000_000_000),
            }),
        ),
    ]);

    let counted: Vec<NumberDataPoint> = pipeline::SIGNALS
        .iter()
        .zip(blocks)
        .filter_map(|(s, n)| n.map(|n| point(n as f64, vec![kv("signal", s)])))
        .collect();
    if !counted.is_empty() {
        metrics.push(gauge(
            "mira.storage.blocks",
            "1",
            "Blocks currently on disk",
            counted,
        ));
    }

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![
                    kv("service.name", "mira"),
                    // The node name, so that replicas sharing a block directory
                    // are separable series rather than one sawtooth. Same
                    // reasoning as the block filename hashing it in.
                    kv("service.instance.id", node),
                    kv("service.version", env!("CARGO_PKG_VERSION")),
                ],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(mira_proto::common::v1::InstrumentationScope {
                    name: SCOPE.into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                }),
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Sample forever, until the metrics pipeline is gone.
///
/// "Gone" in practice means aborted: this task holds an `Ingest` clone, so the
/// flusher cannot close the channel underneath it while it is alive. `serve_with`
/// cancels it as the first step of a stop, and the `Closed` arm below is what
/// covers the other order — a flusher that stopped on its own.
///
/// The first sample waits a whole interval rather than firing at zero. A sample
/// taken during boot is all zeroes and a `free_fraction` read against a
/// directory the flushers have not touched yet; it is not wrong, it is just the
/// least informative point in the series and it is the one a chart's y-axis
/// would scale to.
///
/// Errors are dropped on purpose. A shed self-sample means the node is busy
/// storing real telemetry, which is the correct thing for it to be doing with a
/// full queue, and a warning per interval about it would be this module making
/// noise about its own unimportance.
pub async fn run(
    node: String,
    data_dir: std::path::PathBuf,
    interval: Duration,
    metrics: pipeline::Ingest<ExportMetricsServiceRequest>,
) -> Option<()> {
    let dir = Arc::new(data_dir);
    let start = unix_nanos();
    loop {
        tokio::time::sleep(interval).await;
        let d = Arc::clone(&dir);
        // `scan` and `statfs` are filesystem work, and they go where filesystem
        // work goes for the same reason `/api/v1/stats` sends them there.
        let disk = tokio::task::spawn_blocking(move || {
            (
                mira_core::block::free_fraction(&d).ok(),
                pipeline::SIGNALS
                    .map(|s| mira_core::block::scan(&d, s).ok().map(|b| b.len() as u64)),
            )
        })
        .await
        .ok()?;
        let req = sample(
            &node,
            unix_nanos(),
            start,
            crate::START.elapsed().as_secs(),
            crate::peak_rss(),
            disk.0,
            disk.1,
        );
        if let Err(crate::pipeline::Rejected::Closed) = metrics.submit(req).await {
            return None;
        }
    }
}

fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every point carries the process start, and a restart is therefore
    /// readable as a reset rather than as a counter going backwards for no
    /// reason. This is the whole value of `start_time_unix_nano` and it is one
    /// field, so it is exactly the kind of thing that gets dropped in a refactor.
    #[test]
    fn every_point_carries_the_stream_start_so_a_restart_reads_as_a_reset() {
        let r = sample("n1", 2_000, 1_000, 7, 4096, Some(0.5), [Some(1); 3]);
        let ms = &r.resource_metrics[0].scope_metrics[0].metrics;
        assert!(!ms.is_empty());
        for m in ms {
            let points = match &m.data {
                Some(metric::Data::Sum(s)) => &s.data_points,
                Some(metric::Data::Gauge(g)) => &g.data_points,
                other => panic!("{}: unexpected data {other:?}", m.name),
            };
            assert!(!points.is_empty(), "{} has no points", m.name);
            for p in points {
                assert_eq!(p.start_time_unix_nano, 1_000, "{}", m.name);
                assert_eq!(p.time_unix_nano, 2_000, "{}", m.name);
            }
        }
    }

    /// A reading the node could not take is an absent series, not a zero. A
    /// filesystem that will not answer `statfs` and a filesystem that is full
    /// are opposite operational facts, and a zero says the second one.
    #[test]
    fn a_measurement_that_failed_is_omitted_rather_than_reported_as_zero() {
        let r = sample("n1", 2_000, 1_000, 7, 4096, None, [None; 3]);
        let names: Vec<&str> = r.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(!names.contains(&"mira.storage.free"), "{names:?}");
        assert!(!names.contains(&"mira.storage.blocks"), "{names:?}");
        // And the counters that are always readable are still there, so the
        // absence above is selective rather than the whole sample collapsing.
        assert!(names.contains(&"mira.ingest.rows"), "{names:?}");
    }

    /// One series per counter, with `signal` as an attribute. The alternative —
    /// `mira.ingest.rows.logs` and two siblings — cannot be summed to a node
    /// total without the reader knowing all three names.
    #[test]
    fn a_signal_is_an_attribute_rather_than_three_metric_names() {
        let r = sample("n1", 2_000, 1_000, 7, 4096, Some(0.5), [Some(1); 3]);
        let rows = r.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .find(|m| m.name == "mira.ingest.rows")
            .expect("rows is reported");
        let Some(metric::Data::Sum(s)) = &rows.data else {
            panic!("rows is a sum")
        };
        assert!(s.is_monotonic);
        let mut signals: Vec<&str> = s
            .data_points
            .iter()
            .map(
                |p| match p.attributes[0].value.as_ref().unwrap().value.as_ref() {
                    Some(any_value::Value::StringValue(v)) => v.as_str(),
                    other => panic!("signal is a string, got {other:?}"),
                },
            )
            .collect();
        signals.sort_unstable();
        assert_eq!(signals, ["logs", "metrics", "traces"]);
    }

    /// The open-block series is an age in seconds, and an age is bounded by
    /// `max_block_age` where a Unix timestamp is a diagonal line no axis can
    /// share with anything else.
    #[test]
    fn the_open_block_series_is_an_age_and_a_closed_block_is_zero() {
        let now = 1_000 * 1_000_000_000;
        pipeline::REJECTS[0].open_since.store(990, Relaxed);
        let r = sample("n1", now, 0, 7, 4096, Some(0.5), [Some(1); 3]);
        let m = r.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .find(|m| m.name == "mira.ingest.open_block.age")
            .expect("the open-block age is reported");
        let Some(metric::Data::Gauge(g)) = &m.data else {
            panic!("age is a gauge")
        };
        let value = |i: usize| match g.data_points[i].value {
            Some(number_data_point::Value::AsDouble(v)) => v,
            other => panic!("{other:?}"),
        };
        assert_eq!(value(0), 10.0, "an open block reports how long it has been");
        assert_eq!(value(1), 0.0, "nothing open is 0, not a negative age");
        pipeline::REJECTS[0].open_since.store(0, Relaxed);
    }

    /// The loop, driven through one interval against a queue the test owns.
    ///
    /// Paused time rather than a short interval: tokio advances the clock once
    /// every task is parked, which is exactly the state "the sampler is waiting
    /// for its next tick" is, so the whole interval costs nothing in wall clock
    /// and the test cannot go flaky on a loaded machine.
    #[tokio::test(start_paused = true)]
    async fn the_sampler_writes_one_export_per_interval_and_stops_when_the_pipe_closes() {
        let dir = std::env::temp_dir().join(format!("mira-self-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<pipeline::Job<ExportMetricsServiceRequest>>(1);
        let sampler = tokio::spawn(run(
            "n1".into(),
            dir.clone(),
            Duration::from_secs(60),
            pipeline::Ingest {
                tx,
                rejects: &pipeline::REJECTS[1],
                wal: None,
                signal: mira_core::wal::Signal::Metrics,
            },
        ));

        // One tick, one export. Dropping the job drops its ack channel, which is
        // what a shut-down flusher looks like from here.
        let job = rx.recv().await.expect("one sample per interval");
        drop(job);
        assert_eq!(
            sampler.await.unwrap(),
            None,
            "a closed pipeline ends the loop rather than spinning on it"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
