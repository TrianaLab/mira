//! Kubernetes state, as OTLP logs.
//!
//! The engine holds what the application said about itself. It does not hold
//! why the kubelet killed it, and those are different facts living in different
//! places: an OOM kill is a field on a pod's container status, a failed
//! schedule is an Event, and neither has ever been on the OTLP wire. An RCA
//! written from telemetry alone can name the exception and still miss that the
//! process was terminated for exceeding its memory limit thirty seconds
//! earlier.
//!
//! So the operator reads both and ships them to Mira as log records. Not a
//! second read surface — that was the alternative and architecture section 14
//! rejected it: a separate endpoint the agent has to join against by hand, on
//! timestamps, defeats the point. As logs they land on the one timeline, inside the same
//! `correlate` frame, filterable by the same `query_records` the agent already
//! uses, and expired by the same retention.
//!
//! # Two sources, because Events are not enough
//!
//! `Reason: OOMKilled` is not reliably an Event. It is
//! `status.containerStatuses[].lastState.terminated.reason`, with the exit code
//! beside it, and the same is true of `ImagePullBackOff` and
//! `CreateContainerConfigError` — the Event stream carries a `BackOff` with a
//! message, and the structured reason is only on the pod. Watching Events alone
//! would miss the three most common Kubernetes root causes in the one field
//! that names them.
//!
//! # Nothing here writes
//!
//! Every verb this module needs is `get`, `list` or `watch`. That is the whole
//! of the feature: Mira supplies evidence, and whoever is reading it does the
//! fixing with their own tools. There is deliberately no counterpart to this
//! file that mutates a cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::StreamExt;
use http_body_util::BodyExt;
use k8s_openapi::api::core::v1::{ContainerStatus, Event, Pod};
use k8s_openapi::jiff::Timestamp;
use kube::runtime::watcher::{self, Config};
use kube::{Client, ResourceExt};
use serde_json::json;
use tracing::{debug, warn};

use crate::controller::scoped;

/// OTel severity numbers. Three of the twenty-four, because a Kubernetes object
/// only ever tells us one of three things: this is routine, this is a warning,
/// or this container is dead.
const INFO: i32 = 9;
const WARN: i32 = 13;
const ERROR: i32 = 17;

/// Records buffered before a flush is forced.
///
/// A relisting cluster produces them in bursts and a POST per record would be
/// one HTTP request per Event on a busy cluster. Sized to stay well inside
/// Mira's request limits while still being one request for an ordinary second.
const BATCH: usize = 256;

/// How long a partial batch waits for company.
///
/// This is added to the age of every record in it, so it trades freshness for
/// request count. Two seconds is invisible against an RCA's timeline and turns
/// a trickle of one event per second into one request per two.
const LINGER: Duration = Duration::from_secs(2);

/// How long one export may take before it is abandoned.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Container waiting reasons worth a record.
///
/// The exclusions are the point. `ContainerCreating` and `PodInitializing` are
/// every container that has ever started, and emitting them would bury the
/// seven below in routine traffic that says only "Kubernetes is working". What
/// is here is the set where the container is *not* going to start without
/// somebody changing something.
const STUCK: [&str; 7] = [
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "InvalidImageName",
    "ErrImageNeverPull",
];

/// One log record, before it is OTLP JSON.
///
/// Resource attributes are separate from record attributes because the split is
/// load-bearing downstream, not cosmetic: Mira interns the resource set once per
/// distinct map and derives the entity key from it
/// (`mira_core::identity::resource_key`), so what goes in `resource` decides
/// what correlates and what goes in `attrs` decides what filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// What emitted this — the identifying half.
    pub resource: BTreeMap<String, String>,
    /// When it happened, not when we saw it.
    pub time_nanos: u64,
    pub severity: i32,
    /// `LogRecord.event_name`: the Kubernetes reason, which is exactly what the
    /// OTLP field is for.
    pub event_name: String,
    pub body: String,
    /// The filterable half.
    pub attrs: BTreeMap<String, String>,
}

/// Unix nanoseconds, or `None` for a time before the epoch.
///
/// `jiff`, not chrono: `k8s-openapi` 0.28 wraps its `Time` around
/// `jiff::Timestamp` and re-exports the crate, so this is the API server's own
/// arithmetic rather than a date library of the operator's own. A pre-epoch
/// timestamp is not a clock skew this should paper over with a zero — a record
/// stamped 1970 sits outside every query window there is.
fn nanos(t: &Timestamp) -> Option<u64> {
    u64::try_from(t.as_nanosecond()).ok()
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Resource attributes for the object an Event is about.
///
/// The `k8s.pod.uid` entry is the one that matters and it is not decoration.
/// `mira_core::identity`'s ladder has `k8s.pod.uid` as its second candidate, so
/// a record carrying it gets the *same* entity key as application telemetry
/// from that pod — which is how a CrashLoopBackOff and the spans that stopped
/// arriving end up being one entity rather than two things a reader has to
/// notice are related.
///
/// There is deliberately no synthetic `service.name`. It would be the last
/// candidate in that ladder, so every Deployment-level event in a namespace
/// would collapse onto one entity key, and "everything this thing emitted"
/// would return a plausible subset — the precise failure `identity.rs` opens by
/// warning about. An object with nothing identifying gets `NO_IDENTITY` and the
/// query layer refuses to correlate it, which is the honest answer: a
/// Deployment is not a running thing that emits telemetry.
fn object_resource(kind: &str, name: &str, uid: &str, namespace: &str) -> BTreeMap<String, String> {
    let mut r = BTreeMap::new();
    if !namespace.is_empty() {
        r.insert("k8s.namespace.name".into(), namespace.into());
    }
    if !kind.is_empty() {
        r.insert("k8s.object.kind".into(), kind.into());
    }
    // The semconv key where one exists, so a reader filtering on
    // `k8s.deployment.name` finds these beside everything else that sets it.
    // `k8s.object.name` is the fallback for the long tail of kinds — a CRD, a
    // Gateway — rather than a second spelling of the seven below.
    let key = match kind {
        "Pod" => "k8s.pod.name",
        "Deployment" => "k8s.deployment.name",
        "StatefulSet" => "k8s.statefulset.name",
        "DaemonSet" => "k8s.daemonset.name",
        "ReplicaSet" => "k8s.replicaset.name",
        "Job" => "k8s.job.name",
        "CronJob" => "k8s.cronjob.name",
        "Node" => "k8s.node.name",
        _ => "k8s.object.name",
    };
    if !name.is_empty() {
        r.insert(key.into(), name.into());
    }
    if kind == "Pod" && !uid.is_empty() {
        r.insert("k8s.pod.uid".into(), uid.into());
    }
    r
}

/// One Kubernetes Event as a log record, or `None` if it carries no usable
/// timestamp.
///
/// Dropping the record is deliberate and it is the conservative choice. The
/// alternative is stamping it with the observation time, which puts it in the
/// wrong block, in the wrong query window, and — the part that matters — at the
/// wrong place in an RCA's timeline. A missing line in a postmortem is visible;
/// a line in the wrong order is an argument.
pub fn from_event(e: &Event) -> Option<Record> {
    let at = e
        .event_time
        .as_ref()
        .map(|t| t.0)
        .or_else(|| e.last_timestamp.as_ref().map(|t| t.0))
        .or_else(|| e.first_timestamp.as_ref().map(|t| t.0))
        .or_else(|| e.metadata.creation_timestamp.as_ref().map(|t| t.0))?;

    let o = &e.involved_object;
    let resource = object_resource(
        o.kind.as_deref().unwrap_or_default(),
        o.name.as_deref().unwrap_or_default(),
        o.uid.as_deref().unwrap_or_default(),
        o.namespace.as_deref().unwrap_or_default(),
    );

    let reason = e.reason.clone().unwrap_or_default();
    let mut attrs = BTreeMap::new();
    if !reason.is_empty() {
        // Also on the record and not only in `event_name`, because `event_name`
        // is stored but is not one of the columns the query grammar exposes as
        // a `field` — so the attribute is what a filter can actually reach.
        attrs.insert("k8s.event.reason".into(), reason.clone());
    }
    if let Some(a) = e.action.as_deref().filter(|a| !a.is_empty()) {
        attrs.insert("k8s.event.action".into(), a.into());
    }
    // Only when it is a repeat. A `count` of 1 on every record is a column of
    // ones, and the reader's question is "is this flapping".
    if let Some(n) = e.count.filter(|n| *n > 1) {
        attrs.insert("k8s.event.count".into(), n.to_string());
    }
    if let Some(c) = e
        .reporting_component
        .as_deref()
        .filter(|c| !c.is_empty())
        .or_else(|| e.source.as_ref().and_then(|s| s.component.as_deref()))
    {
        attrs.insert("k8s.event.reporting_controller".into(), c.into());
    }
    if let Some(n) = e.source.as_ref().and_then(|s| s.host.as_deref()) {
        attrs.insert("k8s.node.name".into(), n.into());
    }

    Some(Record {
        resource,
        time_nanos: nanos(&at)?,
        // "Warning" and "Normal" are the only two types the API defines.
        severity: if e.type_.as_deref() == Some("Warning") {
            WARN
        } else {
            INFO
        },
        event_name: reason,
        body: e.message.clone().unwrap_or_default(),
        attrs,
    })
}

/// The reportable state of one container.
///
/// `token` is why this is a struct rather than a record: a watch re-delivers a
/// pod on every unrelated change — a label edit, a condition flipping, another
/// container's probe — and the state we care about is unchanged across almost
/// all of them. Keying on a string that encodes the *state* rather than on the
/// object's resourceVersion means one record per actual transition.
struct State {
    token: String,
    severity: i32,
    reason: String,
    body: String,
    /// When it happened, where the object says. `None` for a state that has no
    /// timestamp anywhere on it, which the caller stamps with the observation.
    at: Option<u64>,
}

fn container_state(cs: &ContainerStatus) -> Option<State> {
    // Checked before `state`, and the order is the whole reason a restart is
    // visible at all: a container that OOMed and was restarted reads as
    // `state.running` with the death recorded only in `lastState`. Reading
    // `state` first would report a healthy container and lose the kill.
    if let Some(t) = cs.last_state.as_ref().and_then(|s| s.terminated.as_ref()) {
        let reason = t.reason.clone().unwrap_or_else(|| "Terminated".into());
        let token = format!(
            "{}/last:{reason}:{}:{}",
            cs.name,
            t.exit_code,
            t.finished_at.as_ref().map(|f| f.0.as_second()).unwrap_or(0)
        );
        let sig = t
            .signal
            .map(|s| format!(", signal {s}"))
            .unwrap_or_default();
        return Some(State {
            token,
            severity: if t.exit_code == 0 { INFO } else { ERROR },
            body: format!(
                "container {} last terminated: exit code {}{sig}{}",
                cs.name,
                t.exit_code,
                t.message
                    .as_deref()
                    .map(|m| format!(" — {m}"))
                    .unwrap_or_default()
            ),
            reason,
            at: t.finished_at.as_ref().and_then(|f| nanos(&f.0)),
        });
    }

    if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
        let reason = w.reason.clone().unwrap_or_default();
        if !STUCK.contains(&reason.as_str()) {
            return None;
        }
        return Some(State {
            token: format!("{}/waiting:{reason}", cs.name),
            severity: ERROR,
            body: format!(
                "container {} is not starting: {reason}{}",
                cs.name,
                w.message
                    .as_deref()
                    .map(|m| format!(" — {m}"))
                    .unwrap_or_default()
            ),
            reason,
            // A waiting container has no timestamp anywhere on the object; the
            // only honest time for it is when we looked. The caller stamps it.
            at: None,
        });
    }

    // Running, and never terminated. Nothing went wrong, so there is nothing to
    // say — the kubelet's own `Started` Event already marks it on the timeline.
    None
}

/// Every reportable container state in `pod`, each with the token that
/// identifies it.
pub fn from_pod(pod: &Pod) -> Vec<(String, Record)> {
    let Some(status) = pod.status.as_ref() else {
        return Vec::new();
    };
    let ns = pod.namespace().unwrap_or_default();
    let name = pod.name_any();
    let uid = pod.uid().unwrap_or_default();
    let resource = object_resource("Pod", &name, &uid, &ns);
    let observed = now_nanos();

    let init = status.init_container_statuses.iter().flatten();
    status
        .container_statuses
        .iter()
        .flatten()
        .chain(init)
        .filter_map(|cs| {
            let s = container_state(cs)?;
            let mut attrs = BTreeMap::from([
                ("k8s.container.name".to_string(), cs.name.clone()),
                (
                    "k8s.container.restart_count".to_string(),
                    cs.restart_count.to_string(),
                ),
                ("k8s.event.reason".to_string(), s.reason.clone()),
            ]);
            if !cs.image.is_empty() {
                attrs.insert("container.image.name".into(), cs.image.clone());
            }
            if let Some(n) = status.host_ip.as_deref().filter(|n| !n.is_empty()) {
                attrs.insert("k8s.pod.host_ip".into(), n.into());
            }
            Some((
                s.token,
                Record {
                    resource: resource.clone(),
                    time_nanos: s.at.unwrap_or(observed),
                    severity: s.severity,
                    event_name: s.reason,
                    body: s.body,
                    attrs,
                },
            ))
        })
        .collect()
}

/// A batch of records as one OTLP/HTTP JSON `ExportLogsServiceRequest`.
///
/// Grouped by resource, and that grouping is not tidiness. Mira stores a
/// `resource_id` per `resourceLogs` entry and the attributes once per resource
/// (section 0, row 2), so a batch sent as one entry per record writes the same
/// four attributes once per record instead of once per pod.
pub fn payload(records: &[Record]) -> serde_json::Value {
    let mut by_resource: BTreeMap<&BTreeMap<String, String>, Vec<&Record>> = BTreeMap::new();
    for r in records {
        by_resource.entry(&r.resource).or_default().push(r);
    }

    let observed = now_nanos().to_string();
    let groups: Vec<_> = by_resource
        .into_iter()
        .map(|(resource, rs)| {
            let logs: Vec<_> = rs
                .iter()
                .map(|r| {
                    json!({
                        "timeUnixNano": r.time_nanos.to_string(),
                        "observedTimeUnixNano": observed,
                        "severityNumber": r.severity,
                        "severityText": match r.severity {
                            ERROR => "ERROR",
                            WARN => "WARN",
                            _ => "INFO",
                        },
                        "eventName": r.event_name,
                        "body": {"stringValue": r.body},
                        "attributes": attributes(&r.attrs),
                    })
                })
                .collect();
            json!({
                "resource": {"attributes": attributes(resource)},
                // The scope names the producer, so `{"attr":"otel.scope.name"}`
                // separates what the operator observed from what an application
                // reported — which is the first thing to establish when the two
                // disagree.
                "scopeLogs": [{
                    "scope": {"name": "mira-operator", "version": env!("CARGO_PKG_VERSION")},
                    "logRecords": logs,
                }],
            })
        })
        .collect();

    json!({ "resourceLogs": groups })
}

fn attributes(m: &BTreeMap<String, String>) -> Vec<serde_json::Value> {
    m.iter()
        .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
        .collect()
}

/// POST one batch to Mira's OTLP/HTTP receiver.
///
/// JSON and not protobuf, which is what keeps this file free: `serde_json` and
/// `hyper-util` are already here for `stats.rs`, and the engine's OTLP JSON
/// decoder is hand-written and already on the other end of `/v1/logs`. A
/// protobuf exporter would mean `prost` and the OTLP schemas in a tree whose
/// whole justification is that the engine's dependency budget stays shut.
///
/// Failure is logged and dropped. There is no retry and no queue: these records
/// describe a cluster that is currently having a problem, and a Mira that
/// cannot take them is part of it. Buffering would trade the operator's memory
/// for records that are stale by the time they land — the same reasoning the
/// alerting webhook states for having no retry
/// ([Connect an agent](../../../docs/agents.md)).
async fn send(http: &HttpClient, endpoint: &str, records: &[Record]) {
    let uri = format!("{}/v1/logs", endpoint.trim_end_matches('/'));
    let body = payload(records).to_string();

    let req = match http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header("content-type", "application/json")
        .body(body)
    {
        Ok(r) => r,
        Err(e) => return warn!("k8s events: unusable endpoint {uri}: {e}"),
    };

    let call = async {
        let res = http.request(req).await.map_err(|e| e.to_string())?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let body = res
            .into_body()
            .collect()
            .await
            .map(|b| String::from_utf8_lossy(&b.to_bytes()).into_owned())
            .unwrap_or_default();
        Err(format!("{status}: {body}"))
    };

    match tokio::time::timeout(SEND_TIMEOUT, call).await {
        Ok(Ok(())) => debug!(records = records.len(), "k8s events exported"),
        Ok(Err(e)) => warn!("k8s events: export to {uri} failed: {e}"),
        Err(_) => warn!("k8s events: export to {uri} timed out"),
    }
}

type HttpClient =
    hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, String>;

/// The environment variable that is the whole switch.
///
/// One value and not two. The chart derives both this and the RBAC from the
/// same `clusterEvents.endpoint`, because an endpoint set without the Role is
/// an operator taking 403s in a loop and a Role without an endpoint is a grant
/// nothing uses — the two-switches-to-get-wrong failure `rbac.yaml` already
/// argues against for the HTTPRoute verbs.
pub const ENDPOINT_ENV: &str = "MIRA_OTLP_ENDPOINT";

/// Start the exporter if an endpoint is configured, on the same namespaces the
/// controller watches.
///
/// Returns immediately when it is not configured, which is the default: no
/// watch is opened, so an operator that was never pointed at a Mira reads
/// nothing and the RBAC the chart did not create is never needed.
pub async fn export(client: Client) {
    let Some(endpoint) = std::env::var(ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return;
    };

    match crate::controller::namespaces(std::env::var("WATCH_NAMESPACES").ok().as_deref()) {
        None => run(client, None, endpoint).await,
        // One watch pair per namespace, mirroring `controller::run` and for the
        // same reason: a namespaced LIST is the only kind a `Role` authorises.
        Some(list) => {
            tracing::info!(namespaces = ?list, endpoint, "exporting Kubernetes state to Mira");
            let each = list
                .into_iter()
                .map(|ns| run(client.clone(), Some(ns), endpoint.clone()));
            futures::future::join_all(each).await;
        }
    }
}

/// Watch Events and pods, and ship what they say to `endpoint`.
///
/// Runs until the process ends. Never returns an error: an export path that
/// took the operator down with it would mean a Mira outage stops the tier being
/// reconciled, and the reconcile is the part that must not depend on this.
async fn run(client: Client, ns: Option<String>, endpoint: String) {
    let http: HttpClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(hyper_util::client::legacy::connect::HttpConnector::new());

    let events = watcher::watcher(
        scoped::<Event>(client.clone(), ns.as_deref()),
        Config::default(),
    );
    let pods = watcher::watcher(scoped::<Pod>(client, ns.as_deref()), Config::default());
    futures::pin_mut!(events, pods);

    // What has already been reported, per pod uid. Bounded by the number of
    // pods in the watch, and entries go when the pod does.
    //
    // ponytail: in-memory, so an operator restart re-reports whatever is
    // *currently* broken once — a duplicate line, not a lost one. Persisting it
    // would mean a store the operator does not otherwise have, for a duplicate
    // that costs one row.
    let mut seen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut batch: Vec<Record> = Vec::new();
    let mut linger = tokio::time::interval(LINGER);

    loop {
        tokio::select! {
            Some(ev) = events.next() => absorb_event(ev, &mut batch),
            Some(ev) = pods.next() => absorb_pod(ev, &mut seen, &mut batch),
            _ = linger.tick() => {
                if !batch.is_empty() {
                    send(&http, &endpoint, &batch).await;
                    batch.clear();
                }
            }
        }

        if batch.len() >= BATCH {
            send(&http, &endpoint, &batch).await;
            batch.clear();
        }
    }
}

/// One step of the Event watch.
///
/// `InitApply` is the relist, not news. Emitting it would re-send every Event
/// still inside the API server's hour-long TTL on each operator restart, and
/// Mira has no dedup key to collapse them with.
///
/// ponytail: so a restart loses the Events that happened while the operator was
/// down. The upgrade path is a resourceVersion checkpoint annotated on the
/// Lease the operator already holds — deferred because the gap is bounded by a
/// restart and the pod watch re-reports any *state* that is still wrong.
///
/// Split out of `run` rather than inlined in the `select!`, because `run` needs
/// an API server on the other end and the decision in here does not.
fn absorb_event(ev: watcher::Result<watcher::Event<Event>>, batch: &mut Vec<Record>) {
    match ev {
        Ok(watcher::Event::Apply(e)) => {
            if let Some(r) = from_event(&e) {
                batch.push(r);
            }
        }
        Err(e) => warn!("k8s events: event watch: {e}"),
        _ => {}
    }
}

/// One step of the pod watch, folded into `seen` with anything newly wrong
/// appended to `batch`.
///
/// A relist seeds the cache without emitting, for the same reason
/// [`absorb_event`] skips it — except that here the cache makes it free rather
/// than lossy: the states are recorded, so the first genuine transition after
/// startup still reports.
fn absorb_pod(
    ev: watcher::Result<watcher::Event<Pod>>,
    seen: &mut BTreeMap<String, BTreeSet<String>>,
    batch: &mut Vec<Record>,
) {
    let (pod, init) = match ev {
        Ok(watcher::Event::Apply(p)) => (Some(p), false),
        Ok(watcher::Event::InitApply(p)) => (Some(p), true),
        Ok(watcher::Event::Delete(p)) => {
            seen.remove(&p.uid().unwrap_or_default());
            (None, false)
        }
        Err(e) => {
            warn!("k8s events: pod watch: {e}");
            (None, false)
        }
        _ => (None, false),
    };
    let Some(p) = pod else { return };
    let known = seen.entry(p.uid().unwrap_or_default()).or_default();
    for (token, record) in from_pod(&p) {
        if known.insert(token) && !init {
            batch.push(record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Objects from JSON rather than from `Default` plus thirty field
    /// assignments: these are the API server's own wire shapes, so writing them
    /// the way the API server sends them is both shorter and the thing actually
    /// under test.
    fn event(v: serde_json::Value) -> Event {
        serde_json::from_value(v).unwrap()
    }
    fn pod(v: serde_json::Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    /// The ordinary case, end to end: a kubelet Warning about a pod becomes a
    /// WARN record whose resource identifies the pod and whose reason is
    /// reachable both as `event_name` and as a filterable attribute.
    #[test]
    fn a_warning_event_carries_its_reason_as_both_a_name_and_an_attribute() {
        let r = from_event(&event(serde_json::json!({
            "metadata": {"name": "x", "namespace": "shop"},
            "type": "Warning",
            "reason": "FailedScheduling",
            "message": "0/3 nodes are available: insufficient memory",
            "eventTime": "2026-09-19T10:00:00.000000Z",
            "involvedObject": {"kind": "Pod", "name": "payments-0",
                               "namespace": "shop", "uid": "pod-uid-1"},
            "reportingComponent": "default-scheduler",
        })))
        .expect("an event with a time is a record");

        assert_eq!(r.severity, WARN);
        assert_eq!(r.event_name, "FailedScheduling");
        assert_eq!(r.body, "0/3 nodes are available: insufficient memory");
        assert_eq!(
            r.attrs.get("k8s.event.reason").map(String::as_str),
            Some("FailedScheduling")
        );
        assert_eq!(
            r.attrs
                .get("k8s.event.reporting_controller")
                .map(String::as_str),
            Some("default-scheduler")
        );
        assert_eq!(
            r.resource.get("k8s.pod.name").map(String::as_str),
            Some("payments-0")
        );
    }

    /// The entity join, as a test. `k8s.pod.uid` is the second candidate in
    /// `mira_core::identity`'s ladder, so its presence is what makes this record
    /// the *same* entity as the spans from that pod. Dropping it would not fail
    /// any query — it would silently split one pod into two entities.
    #[test]
    fn a_pod_event_carries_the_uid_that_joins_it_to_the_pods_own_telemetry() {
        let r = from_event(&event(serde_json::json!({
            "metadata": {"name": "x", "namespace": "shop"},
            "reason": "Killing",
            "lastTimestamp": "2026-09-19T10:00:00Z",
            "involvedObject": {"kind": "Pod", "name": "payments-0",
                               "namespace": "shop", "uid": "pod-uid-1"},
        })))
        .unwrap();
        assert_eq!(
            r.resource.get("k8s.pod.uid").map(String::as_str),
            Some("pod-uid-1")
        );
    }

    /// And the other half of that decision: no synthetic `service.name`. It is
    /// the last candidate in the identity ladder, so setting it would collapse
    /// every Deployment-level event in a namespace onto one entity key and make
    /// "everything this thing emitted" return a plausible subset.
    #[test]
    fn nothing_invents_a_service_name() {
        for kind in ["Pod", "Deployment", "Node", "SomeCRD"] {
            let r = from_event(&event(serde_json::json!({
                "metadata": {"name": "x", "namespace": "shop"},
                "reason": "Whatever",
                "lastTimestamp": "2026-09-19T10:00:00Z",
                "involvedObject": {"kind": kind, "name": "n", "namespace": "shop"},
            })))
            .unwrap();
            assert!(!r.resource.contains_key("service.name"), "{kind}");
        }
    }

    /// The timestamp ladder, and the refusal at the end of it. A record with no
    /// time of its own must be dropped rather than stamped with the observation
    /// — a line in the wrong place in an RCA's timeline is an argument, where a
    /// missing one is visible.
    #[test]
    fn an_event_with_no_timestamp_anywhere_is_dropped_not_stamped_with_now() {
        let base = serde_json::json!({
            "metadata": {"name": "x", "namespace": "shop"},
            "reason": "Pulled",
            "involvedObject": {"kind": "Pod", "name": "p", "namespace": "shop"},
        });
        assert!(from_event(&event(base.clone())).is_none());

        // Each rung of the ladder is enough on its own.
        for key in ["eventTime", "lastTimestamp", "firstTimestamp"] {
            let mut v = base.clone();
            let t = if key == "eventTime" {
                "2026-09-19T10:00:00.000000Z"
            } else {
                "2026-09-19T10:00:00Z"
            };
            v[key] = serde_json::json!(t);
            assert!(from_event(&event(v)).is_some(), "{key}");
        }
    }

    /// The reason this module watches pods at all. A container that OOMed and
    /// was restarted reads as `state.running`, with the kill recorded only in
    /// `lastState` — so reading `state` first reports a healthy container and
    /// loses the single most useful fact in the object.
    #[test]
    fn an_oomkill_behind_a_running_container_is_still_reported() {
        let out = from_pod(&pod(serde_json::json!({
            "metadata": {"name": "payments-0", "namespace": "shop", "uid": "u1"},
            "status": {"containerStatuses": [{
                "name": "payments", "image": "shop/payments:2.7.0",
                "ready": true, "restartCount": 3,
                "state": {"running": {"startedAt": "2026-09-19T10:00:05Z"}},
                "lastState": {"terminated": {
                    "exitCode": 137, "reason": "OOMKilled",
                    "finishedAt": "2026-09-19T10:00:00Z", "startedAt": "2026-09-19T09:00:00Z",
                }},
            }]},
        })));

        assert_eq!(out.len(), 1);
        let (_, r) = &out[0];
        assert_eq!(r.severity, ERROR);
        assert_eq!(r.event_name, "OOMKilled");
        assert!(r.body.contains("137"), "{}", r.body);
        assert_eq!(
            r.attrs
                .get("k8s.container.restart_count")
                .map(String::as_str),
            Some("3")
        );
        // The death, not the observation: the record has to sort into the
        // window the incident happened in.
        // 2026-09-19T10:00:00Z, the `finishedAt` above.
        assert_eq!(r.time_nanos, 1_789_812_000_000_000_000);
    }

    /// `ContainerCreating` and `PodInitializing` are every container that has
    /// ever started. Emitting them would bury the states that mean something.
    #[test]
    fn a_routine_waiting_state_says_nothing_and_a_stuck_one_is_an_error() {
        let waiting = |reason: &str| {
            from_pod(&pod(serde_json::json!({
                "metadata": {"name": "p", "namespace": "shop", "uid": "u1"},
                "status": {"containerStatuses": [{
                    "name": "c", "image": "i", "ready": false, "restartCount": 0,
                    "state": {"waiting": {"reason": reason, "message": "back-off 5m0s"}},
                }]},
            })))
        };

        assert!(waiting("ContainerCreating").is_empty());
        assert!(waiting("PodInitializing").is_empty());

        let out = waiting("CrashLoopBackOff");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.severity, ERROR);
        assert_eq!(out[0].1.event_name, "CrashLoopBackOff");
    }

    /// The dedup key. A watch re-delivers a pod on every unrelated change, so
    /// the token has to be stable across them and change when the state does —
    /// otherwise the exporter either floods or goes silent.
    #[test]
    fn the_token_is_stable_across_noise_and_moves_on_a_real_transition() {
        let at = |restarts: i32, exit: i32| {
            let out = from_pod(&pod(serde_json::json!({
                "metadata": {"name": "p", "namespace": "shop", "uid": "u1",
                             // The kind of unrelated churn a watch delivers.
                             "resourceVersion": restarts.to_string(),
                             "labels": {"rev": restarts.to_string()}},
                "status": {"containerStatuses": [{
                    "name": "c", "image": "i", "ready": true, "restartCount": restarts,
                    "lastState": {"terminated": {
                        "exitCode": exit, "reason": "Error",
                        "finishedAt": "2026-09-19T10:00:00Z", "startedAt": "2026-09-19T09:00:00Z",
                    }},
                }]},
            })));
            out[0].0.clone()
        };

        // Same death, different resourceVersion and label: one record, not two.
        assert_eq!(at(1, 1), at(1, 1));
        // A different exit code is a different death.
        assert_ne!(at(1, 1), at(1, 2));
    }

    /// Grouping is not tidiness. Mira stores the resource attributes once per
    /// `resourceLogs` entry, so a batch sent one-entry-per-record writes the
    /// same four attributes once per record instead of once per pod.
    #[test]
    fn the_payload_groups_records_by_resource() {
        let rec = |pod: &str, body: &str| Record {
            resource: BTreeMap::from([("k8s.pod.name".to_string(), pod.to_string())]),
            time_nanos: 1,
            severity: INFO,
            event_name: "R".into(),
            body: body.into(),
            attrs: BTreeMap::new(),
        };
        let p = payload(&[rec("a", "1"), rec("b", "1"), rec("a", "2")]);

        let groups = p["resourceLogs"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "two pods, two resources");
        let counts: Vec<usize> = groups
            .iter()
            .map(|g| g["scopeLogs"][0]["logRecords"].as_array().unwrap().len())
            .collect();
        assert_eq!(counts, vec![2, 1]);
        assert_eq!(
            groups[0]["scopeLogs"][0]["scope"]["name"].as_str(),
            Some("mira-operator")
        );
    }

    /// The two spellings the engine's decoder is strict about: a 64-bit
    /// timestamp is a *string* in OTLP JSON because a double cannot hold one
    /// exactly, and the severity is a number. Getting either wrong is a 400 from
    /// a receiver that is otherwise working.
    #[test]
    fn the_payload_matches_what_the_otlp_json_decoder_expects() {
        let p = payload(&[Record {
            resource: BTreeMap::from([("k8s.pod.name".to_string(), "p".to_string())]),
            time_nanos: 1_789_812_000_000_000_000,
            severity: ERROR,
            event_name: "OOMKilled".into(),
            body: "dead".into(),
            attrs: BTreeMap::from([("k8s.container.name".to_string(), "c".to_string())]),
        }]);
        let r = &p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];

        assert_eq!(r["timeUnixNano"].as_str(), Some("1789812000000000000"));
        assert_eq!(r["severityNumber"].as_i64(), Some(17));
        assert_eq!(r["severityText"].as_str(), Some("ERROR"));
        assert_eq!(r["body"]["stringValue"].as_str(), Some("dead"));
        assert_eq!(r["eventName"].as_str(), Some("OOMKilled"));
        assert_eq!(
            r["attributes"][0]["key"].as_str(),
            Some("k8s.container.name")
        );
        assert_eq!(
            r["attributes"][0]["value"]["stringValue"].as_str(),
            Some("c")
        );
    }
    /// The fields that are only on the record when the API server actually set
    /// them. Each one is a `filter` that a reader would not notice was wrong:
    /// a `count` of 1 on every row is a column of ones, an empty `action` is a
    /// column of blanks, and both cost a column in every block.
    #[test]
    fn the_optional_event_fields_are_present_only_when_they_say_something() {
        let bare = from_event(&event(serde_json::json!({
            "metadata": {"name": "x", "namespace": "shop"},
            "reason": "Killing",
            "action": "",
            "count": 1,
            "lastTimestamp": "2026-09-19T10:00:00Z",
            "involvedObject": {"kind": "Pod", "name": "p", "namespace": "shop"},
        })))
        .unwrap();
        assert!(
            !bare.attrs.contains_key("k8s.event.action"),
            "{:?}",
            bare.attrs
        );
        assert!(
            !bare.attrs.contains_key("k8s.event.count"),
            "{:?}",
            bare.attrs
        );
        assert!(
            !bare.attrs.contains_key("k8s.node.name"),
            "{:?}",
            bare.attrs
        );

        let full = from_event(&event(serde_json::json!({
            "metadata": {"name": "x", "namespace": "shop"},
            "reason": "BackOff",
            "action": "Pulling",
            "count": 7,
            "lastTimestamp": "2026-09-19T10:00:00Z",
            "involvedObject": {"kind": "Pod", "name": "p", "namespace": "shop"},
            "source": {"component": "kubelet", "host": "node-3"},
        })))
        .unwrap();
        assert_eq!(
            full.attrs.get("k8s.event.action").map(String::as_str),
            Some("Pulling")
        );
        assert_eq!(
            full.attrs.get("k8s.event.count").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            full.attrs.get("k8s.node.name").map(String::as_str),
            Some("node-3")
        );
        // `source.component` is the fallback for the field the modern Event
        // spells `reportingComponent`, so an old-style Event is not anonymous.
        assert_eq!(
            full.attrs
                .get("k8s.event.reporting_controller")
                .map(String::as_str),
            Some("kubelet")
        );
    }

    /// A pod with no `status` at all — the first apply of a pod the scheduler
    /// has not touched yet — has nothing to report and must not be an empty
    /// record with a made-up reason.
    #[test]
    fn a_pod_with_no_status_yet_reports_nothing() {
        assert!(
            from_pod(&pod(serde_json::json!({
                "metadata": {"name": "p", "namespace": "shop", "uid": "u"}
            })))
            .is_empty()
        );
    }

    /// `hostIP` is the one field here that answers "which node", and a running
    /// container answers nothing at all.
    #[test]
    fn a_reported_container_carries_its_node_and_a_healthy_one_is_silent() {
        let out = from_pod(&pod(serde_json::json!({
            "metadata": {"name": "p", "namespace": "shop", "uid": "u"},
            "status": {
                "hostIP": "10.0.0.7",
                "containerStatuses": [
                    {"name": "dead", "image": "app:1.4.0", "ready": false, "restartCount": 2,
                     "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137,
                                                  "finishedAt": "2026-09-19T10:00:00Z"}},
                     "state": {"running": {"startedAt": "2026-09-19T10:00:05Z"}}},
                    {"name": "fine", "image": "side:1", "ready": true, "restartCount": 0,
                     "state": {"running": {"startedAt": "2026-09-19T09:00:00Z"}}}
                ]
            }
        })));
        assert_eq!(out.len(), 1, "only the one that went wrong: {out:?}");
        assert_eq!(
            out[0].1.attrs.get("k8s.pod.host_ip").map(String::as_str),
            Some("10.0.0.7")
        );
    }

    /// Severity survives into the wire shape as both a number and the text OTLP
    /// pairs with it. A WARN written as `"INFO"` is a row that every
    /// severity filter in Mira disagrees with.
    #[test]
    fn every_severity_writes_the_text_that_matches_its_number() {
        let rec = |severity| Record {
            resource: BTreeMap::from([("k8s.pod.uid".to_string(), "u".to_string())]),
            time_nanos: 1,
            severity,
            event_name: "R".into(),
            body: "b".into(),
            attrs: BTreeMap::new(),
        };
        for (severity, text) in [(INFO, "INFO"), (WARN, "WARN"), (ERROR, "ERROR")] {
            let p = payload(&[rec(severity)]);
            let r = &p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
            assert_eq!(r["severityNumber"].as_i64(), Some(severity as i64));
            assert_eq!(r["severityText"].as_str(), Some(text));
        }
    }

    /// The relist is the case that decides whether an operator restart is
    /// quiet. It has to seed the cache — so the *next* apply of the same
    /// broken container is still a duplicate — without emitting anything.
    #[test]
    fn a_relist_seeds_the_dedup_cache_without_reporting_what_it_found() {
        let broken = || {
            pod(serde_json::json!({
                "metadata": {"name": "p", "namespace": "shop", "uid": "u"},
                "status": {"containerStatuses": [
                    {"name": "c", "image": "app:1", "ready": false, "restartCount": 1,
                     "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137,
                                                  "finishedAt": "2026-09-19T10:00:00Z"}},
                     "state": {"running": {"startedAt": "2026-09-19T10:00:05Z"}}}
                ]}
            }))
        };
        let mut seen = BTreeMap::new();
        let mut batch = Vec::new();

        absorb_pod(
            Ok(watcher::Event::InitApply(broken())),
            &mut seen,
            &mut batch,
        );
        assert!(batch.is_empty(), "a relist says nothing: {batch:?}");
        assert_eq!(seen["u"].len(), 1, "but it remembers");

        absorb_pod(Ok(watcher::Event::Apply(broken())), &mut seen, &mut batch);
        assert!(batch.is_empty(), "the same kill again is the same kill");

        // A second kill, with a different token, is news.
        let again = pod(serde_json::json!({
            "metadata": {"name": "p", "namespace": "shop", "uid": "u"},
            "status": {"containerStatuses": [
                {"name": "c", "image": "app:1", "ready": false, "restartCount": 2,
                 "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137,
                                              "finishedAt": "2026-09-19T10:04:00Z"}},
                 "state": {"running": {"startedAt": "2026-09-19T10:04:05Z"}}}
            ]}
        }));
        absorb_pod(Ok(watcher::Event::Apply(again)), &mut seen, &mut batch);
        assert_eq!(batch.len(), 1, "a second kill is a second record");

        // And the pod going away takes its history with it, so the uid map is
        // bounded by the pods in the watch rather than by the pods there have
        // ever been.
        absorb_pod(Ok(watcher::Event::Delete(broken())), &mut seen, &mut batch);
        assert!(seen.is_empty(), "{seen:?}");
    }

    /// The Event arm of the watch, over both ends of the timestamp ladder: an
    /// Event with nothing on it is dropped rather than stamped with `now`,
    /// because a line in the wrong place in a timeline is worse than a line
    /// that is not there — and `creationTimestamp`, the last rung, still
    /// places one.
    #[test]
    fn the_event_arm_drops_what_it_cannot_place_and_keeps_what_it_can() {
        let mut batch = Vec::new();
        absorb_event(
            Ok(watcher::Event::Apply(event(serde_json::json!({
                "metadata": {"name": "x", "namespace": "shop"},
                "reason": "Killing",
                "involvedObject": {"kind": "Pod", "name": "p", "namespace": "shop"},
            })))),
            &mut batch,
        );
        assert!(batch.is_empty(), "no time, no line: {batch:?}");

        absorb_event(
            Ok(watcher::Event::Apply(event(serde_json::json!({
                "metadata": {"name": "x", "namespace": "shop",
                             "creationTimestamp": "2026-09-19T10:00:00Z"},
                "reason": "Killing",
                "involvedObject": {"kind": "Pod", "name": "p", "namespace": "shop"},
            })))),
            &mut batch,
        );
        assert_eq!(batch.len(), 1, "the last rung of the ladder still counts");
    }

    /// A one-shot HTTP/1.1 server. Hand-rolled rather than `hyper`'s: the
    /// operator reaches hyper only through `hyper-util`'s client, and a
    /// dev-dependency on `hyper/server` to assert one POST is a crate in the
    /// lockfile for the length of a test.
    async fn one_shot(status: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 8192];
            // Read until the body is whole: the headers end at the blank line
            // and `content-length` says how much follows.
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).into_owned();
                if let Some(head) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .to_ascii_lowercase()
                        .split("content-length:")
                        .nth(1)
                        .and_then(|t| t.split("\r\n").next())
                        .and_then(|t| t.trim().parse().ok())
                        .unwrap_or_default();
                    if got.len() >= head + 4 + len {
                        break;
                    }
                }
                assert!(n > 0, "the client hung up before the body");
            }
            sock.write_all(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });
        (format!("http://{addr}"), served)
    }

    fn client() -> HttpClient {
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(hyper_util::client::legacy::connect::HttpConnector::new())
    }

    fn one_record() -> Record {
        Record {
            resource: BTreeMap::from([("k8s.pod.uid".to_string(), "u".to_string())]),
            time_nanos: 1_789_812_000_000_000_000,
            severity: ERROR,
            event_name: "OOMKilled".into(),
            body: "container c was killed".into(),
            attrs: BTreeMap::from([("k8s.container.name".to_string(), "c".to_string())]),
        }
    }

    /// The wire, once: the batch arrives at `/v1/logs` as JSON, on the path the
    /// engine's OTLP receiver actually serves. A trailing slash on the endpoint
    /// is the configuration mistake this would otherwise turn into a 404 loop.
    #[tokio::test]
    async fn a_batch_is_posted_to_v1_logs_as_otlp_json() {
        let (endpoint, served) = one_shot("200 OK").await;
        send(&client(), &format!("{endpoint}/"), &[one_record()]).await;

        let raw = served.await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /v1/logs "), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json"),
            "{head}"
        );
        let sent: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            sent["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["eventName"].as_str(),
            Some("OOMKilled")
        );
    }

    /// A Mira that refuses the batch is logged and dropped. There is no retry
    /// and no queue by design — these records describe a cluster that is
    /// currently having a problem, and buffering them would trade the
    /// operator's memory for rows that are stale when they land.
    #[tokio::test]
    async fn a_refused_batch_is_dropped_rather_than_retried() {
        let (endpoint, served) = one_shot("503 Service Unavailable").await;
        send(&client(), &endpoint, &[one_record()]).await;
        // One request reached the server and `send` returned rather than
        // looping: the join resolves, and nothing else connects.
        served.await.unwrap();
    }

    /// An endpoint the URI grammar rejects fails once, at the builder, instead
    /// of once per batch for the life of the process.
    #[tokio::test]
    async fn an_unusable_endpoint_fails_at_the_request_rather_than_on_the_wire() {
        send(&client(), "http://not a host", &[one_record()]).await;
    }
}
