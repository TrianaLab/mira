//! The objects the operator owns, built from a `MiraCluster`.
//!
//! Written as `json!` literals deserialised into the k8s-openapi types rather
//! than as struct initialisers. The structs are ~40 `Option` fields deep for a
//! pod spec and the initialiser form buries the eight fields that matter under
//! ~200 `..Default::default()` lines, which is how a chart-shaped object stops
//! being reviewable against the chart it has to match.
//!
//! The cost of that choice is real and worth naming: k8s-openapi's types ignore
//! unknown fields, so a typo'd key deserialises cleanly and vanishes. The
//! `json!` blocks below are therefore paired with round-trip tests over exactly
//! the fields whose loss would be silent in the cluster rather than loud —
//! `volumeClaimTemplates`, the readiness probe, the data mount. A typo in one
//! of those is a tier that comes up and then loses its blocks on restart.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{DynamicObject, ObjectMeta, TypeMeta};
use kube::{Resource, ResourceExt};
use serde_json::json;

use crate::crd::{MiraCluster, Route};

/// Ports, named once. The engine hardcodes them in the chart too.
pub const GRPC: i32 = 4317;
pub const HTTP: i32 = 4318;

/// `app.kubernetes.io` labels, and the selector subset of them.
///
/// Split because a StatefulSet's `spec.selector` is immutable after creation.
/// Putting the version in the selector — the mistake the full label set invites
/// — makes the first image bump an un-upgradeable object that has to be deleted
/// by hand, taking its PVCs' owner references with it.
pub fn selector(c: &MiraCluster) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "mira".into()),
        ("app.kubernetes.io/instance".into(), c.name_any()),
    ])
}

fn labels(c: &MiraCluster, component: &str) -> BTreeMap<String, String> {
    let mut l = selector(c);
    l.insert("app.kubernetes.io/component".into(), component.into());
    l.insert(
        "app.kubernetes.io/managed-by".into(),
        "mira-operator".into(),
    );
    l
}

fn proxy_selector(c: &MiraCluster) -> BTreeMap<String, String> {
    let mut l = selector(c);
    l.insert("app.kubernetes.io/component".into(), "proxy".into());
    l
}

/// The storage pods, and nothing else the cluster owns.
///
/// [`selector`] alone is not this. It is the StatefulSet's `matchLabels` — the
/// immutable subset, deliberately narrow so a later label can be added without
/// a recreate — and every pod under this `MiraCluster` carries it, including
/// the proxy's and the drain Job's. Anything matching *pods* needs the
/// component too.
fn storage_selector(c: &MiraCluster) -> BTreeMap<String, String> {
    let mut l = selector(c);
    l.insert("app.kubernetes.io/component".into(), "storage".into());
    l
}

/// Owned-by metadata, so deleting the `MiraCluster` collects everything.
///
/// Every object below carries it. Without it, deleting a cluster leaves a
/// StatefulSet running and a proxy serving from it, and the only trace of why
/// is a name that happens to match.
fn meta(c: &MiraCluster, name: String, component: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: c.namespace(),
        labels: Some(labels(c, component)),
        owner_references: Some(vec![c.controller_owner_ref(&()).expect("cluster is named")]),
        ..Default::default()
    }
}

pub fn headless_name(c: &MiraCluster) -> String {
    format!("{}-headless", c.name_any())
}
pub fn proxy_name(c: &MiraCluster) -> String {
    format!("{}-proxy", c.name_any())
}

/// Stable DNS for one storage replica.
///
/// This is the address the proxy is given and the address the operator reads
/// stats from. It is a *pod* DNS name off the headless Service rather than the
/// Service itself, because both callers need to reach one specific replica —
/// the whole point of the proxy is that it talks to each one in turn, and a
/// load-balanced name would make "replica 3's free space" unanswerable.
pub fn replica_host(c: &MiraCluster, ordinal: i32) -> String {
    format!(
        "{}-{}.{}.{}.svc",
        c.name_any(),
        ordinal,
        headless_name(c),
        c.namespace().unwrap_or_else(|| "default".into())
    )
}

/// The storage node config, as the KYAML the engine parses.
///
/// `${env:POD_NAME}` and not the ordinal: the node name is hashed into the
/// block directory name so replicas sharing a volume cannot collide, and it has
/// to survive a reschedule. The downward API is the only thing that promises
/// that — `HOSTNAME` is set by the container runtime, not by Kubernetes.
///
/// **`spec.offload` is deliberately not here**, and putting it here is the
/// third time this shape of bug has been written in this file. `storage.offload`
/// on a *running* node is a retention setting: `expire_with` copies each
/// expiring block to the target and then unlinks the local one, so the target
/// has to be a mount. The replicas mount `data` and `config` and nothing else,
/// which would make `/cold/tel-0` the container's own writable layer — blocks
/// leaving the claim for a directory that dies with the pod, with the same
/// `Ready` status and the same silent loss that `coldStorageClaim` exists to
/// make unrepresentable, just moved from the drain Job to the replica.
///
/// It is not an oversight in the mounts either: every replica would have to
/// hold the cold claim at once, which makes `ReadWriteMany` mandatory for any
/// tier of more than one pod on more than one node. So `spec.offload` means
/// what the CRD says it means — where a drain archives to, passed to the Job on
/// its command line — and nothing else reads it.
///
/// ponytail: no retention tiering to cold storage from a live replica. The
/// upgrade path is a separate `spec.coldTiering` that mounts the claim into the
/// StatefulSet and documents the RWX requirement, not a second meaning for this
/// field.
fn node_config() -> String {
    format!(
        r#"{{
  "node": "${{env:POD_NAME}}",
  "listen": {{
    "grpc": "0.0.0.0:{GRPC}",
    "http": "0.0.0.0:{HTTP}",
  }},
  "storage": {{
    "dir": "/data",
  }},
}}
"#
    )
}

pub fn config_map(c: &MiraCluster) -> ConfigMap {
    ConfigMap {
        metadata: meta(c, c.name_any(), "storage"),
        data: Some(BTreeMap::from([("mira.yaml".into(), node_config())])),
        ..Default::default()
    }
}

/// The proxy's config: the replica list, and nothing else it could disagree
/// with the storage nodes about.
///
/// Regenerated on every reconcile from the *current* replica count, which is
/// what makes a scale event reach the proxy at all. The engine reads this list
/// once at boot and `route`'s `% n` is computed against it, so the Deployment
/// below hashes this config into a pod annotation — a replica count that
/// changed without restarting the proxy is a proxy fanning out to a pod that no
/// longer exists, or missing one that does.
fn proxy_config(c: &MiraCluster, replicas: i32) -> String {
    let list = (0..replicas)
        .map(|i| format!("http://{}:{HTTP}", replica_host(c, i)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"{{
  "listen": {{
    "http": "0.0.0.0:{HTTP}",
  }},
  "proxy": {{
    "replicas": {},
  }},
}}
"#,
        json!(list)
    )
}

pub fn proxy_config_map(c: &MiraCluster, replicas: i32) -> ConfigMap {
    ConfigMap {
        metadata: meta(c, proxy_name(c), "proxy"),
        data: Some(BTreeMap::from([(
            "mira.yaml".into(),
            proxy_config(c, replicas),
        )])),
        ..Default::default()
    }
}

/// A cheap, stable hash of the config, for the restart annotation.
///
/// Not a cryptographic digest and it does not need to be: the only requirement
/// is that a changed replica list changes the string, so the Deployment's pod
/// template changes and Kubernetes rolls it. Pulling in a sha2 crate to restart
/// a pod would be a dependency bought with nothing.
fn config_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn headless_service(c: &MiraCluster) -> Service {
    serde_json::from_value(json!({
        "metadata": meta(c, headless_name(c), "storage"),
        "spec": {
            "clusterIP": "None",
            // So a pod gets DNS before it is Ready. The operator reads stats
            // from these names, and a replica that is starting up is exactly
            // the one whose name has to resolve so the read can fail honestly
            // as Unreachable rather than as NXDOMAIN.
            "publishNotReadyAddresses": true,
            // `storage_selector` and not `selector`: the narrow one matches
            // every pod the cluster owns, so the proxy landed in this Service's
            // endpoints too. `tel-0.tel-headless` comes from the StatefulSet's
            // `serviceName` and so kept resolving correctly, which is why it
            // took an `HTTPRoute` aimed at the Service name to see it.
            "selector": storage_selector(c),
            "ports": [
                {"name": "otlp-grpc", "port": GRPC, "targetPort": GRPC, "protocol": "TCP"},
                {"name": "otlp-http", "port": HTTP, "targetPort": HTTP, "protocol": "TCP"},
            ],
        },
    }))
    .expect("headless service is well-formed")
}

pub fn proxy_service(c: &MiraCluster) -> Service {
    serde_json::from_value(json!({
        "metadata": meta(c, proxy_name(c), "proxy"),
        "spec": {
            "type": "ClusterIP",
            "selector": proxy_selector(c),
            "ports": [
                {"name": "otlp-http", "port": HTTP, "targetPort": HTTP, "protocol": "TCP"},
            ],
        },
    }))
    .expect("proxy service is well-formed")
}

/// The `HTTPRoute` in front of the tier, if the spec asked for one.
///
/// A [`DynamicObject`] and not a typed struct, because Gateway API is a CRD
/// rather than core Kubernetes and k8s-openapi therefore does not carry it. The
/// alternative is the `gateway-api` crate: a dependency, a second Kubernetes
/// type tree to keep in step, and a second spelling of the four fields this
/// function writes by hand anyway.
///
/// `v1` and not `v1beta1`: the kinds have shared a storage version since
/// Gateway API 1.0, and `v1` is the one every cluster that has the CRDs at all
/// will serve.
///
/// # What it routes where
///
/// Two backends, because the tier has two kinds of address and they answer
/// different questions. `/v1/*` is OTLP ingest and `/api/v1/query` is the read
/// a proxy merges across replicas, so both go to the proxy. The UI, `/mcp` and
/// the reads built by walking one node's blocks are answered by a storage node
/// and 501 by the proxy, so everything else goes to the headless Service.
///
/// Rules are matched most-specific-first by the Gateway API rather than in
/// order, so the catch-all being last is documentation.
///
/// With `proxy.replicas: 0` there is no proxy to send anything to and the
/// catch-all is the whole route. That tier ingests through the headless Service
/// — round-robin across replicas, which is what `route` on each node already
/// assumes.
pub fn http_route(c: &MiraCluster, r: &Route) -> DynamicObject {
    let merged = json!({
        "matches": [
            {"path": {"type": "PathPrefix", "value": "/v1/"}},
            {"path": {"type": "Exact", "value": "/api/v1/query"}},
        ],
        "backendRefs": [{"name": proxy_name(c), "port": HTTP}],
    });
    let everything_else = json!({
        "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
        "backendRefs": [{"name": headless_name(c), "port": HTTP}],
    });
    let rules: Vec<_> = (c.spec.proxy.replicas > 0)
        .then_some(merged)
        .into_iter()
        .chain([everything_else])
        .collect();

    let mut spec = json!({
        // Serialised straight from the CRD type: `ParentRef`'s three fields are
        // already the Gateway API's spelling, and mapping them by hand here
        // would be a second place for `sectionName` to be wrong.
        "parentRefs": r.parent_refs,
        "rules": rules,
    });
    if !r.hostnames.is_empty() {
        spec["hostnames"] = json!(r.hostnames);
    }

    DynamicObject {
        types: Some(TypeMeta {
            api_version: "gateway.networking.k8s.io/v1".into(),
            kind: "HTTPRoute".into(),
        }),
        metadata: meta(c, c.name_any(), "route"),
        data: json!({ "spec": spec }),
    }
}

/// What a node drain is allowed to take from the tier at once.
///
/// One replica. Its volume holds blocks no other replica has, nothing
/// replicates and nothing rebalances (docs/architecture/replicas-scaling.md section 12.4), so an
/// eviction is a hole in the corpus until the claim rebinds and the pod
/// replays. Two at once is two holes, and a node running two of the tier's
/// pods is the normal case rather than an unlucky one.
///
/// `maxUnavailable` and not `minAvailable`, which reads more naturally and is
/// wrong here: `minAvailable: 1` against the single-replica tier that is the
/// CRD's default permits no disruption at all, and a node drain then blocks
/// for ever on a pod the eviction API will never release.
pub fn pod_disruption_budget(c: &MiraCluster) -> PodDisruptionBudget {
    serde_json::from_value(json!({
        "metadata": meta(c, c.name_any(), "storage"),
        "spec": {
            "maxUnavailable": 1,
            // The storage pods, and only those. A budget selects pods, and the
            // StatefulSet's own `matchLabels` is a *subset* match that also
            // catches the proxy's pods and the drain Job's — so one budget of
            // one covered the whole cluster, and a node drain could be refused
            // because a stateless proxy replica was restarting, or a drain Job
            // the operator itself created was still copying.
            "selector": {"matchLabels": storage_selector(c)},
        },
    }))
    .expect("pod disruption budget is well-formed")
}

/// The pod-level hardening every pod in this module gets.
///
/// `fsGroup` is the load-bearing one and the reason this is not left to the
/// image: a freshly provisioned PVC is mounted root-owned, and without a group
/// for the CSI driver to chown it to, the engine's first block directory is a
/// permission error minutes after a pod that looked healthy — the failure
/// `mira_core::error` names in the message it prints.
///
/// `runAsUser` is spelled out beside `runAsNonRoot` rather than inherited from
/// the image because the Dockerfile's `USER nonroot:nonroot` is a *name*. The
/// kubelet cannot prove a name is not root, so `runAsNonRoot` on its own
/// refuses to start the container rather than allowing it. 65532 is that name's
/// uid in the distroless base and the owner of `/data` in the image.
fn pod_security_context() -> serde_json::Value {
    json!({
        "runAsNonRoot": true,
        "runAsUser": 65532,
        "runAsGroup": 65532,
        "fsGroup": 65532,
        // Mira makes ordinary syscalls — mmap, statfs, fsync, sockets — and
        // needs no exemption from the default filter. Named because the
        // `restricted` Pod Security Standard rejects a pod that leaves it unset.
        "seccompProfile": {"type": "RuntimeDefault"},
    })
}

/// Nothing in the image is setuid and there is no shell to escalate into.
fn container_security_context() -> serde_json::Value {
    json!({"allowPrivilegeEscalation": false})
}

pub fn stateful_set(c: &MiraCluster, replicas: i32) -> StatefulSet {
    serde_json::from_value(json!({
        "metadata": meta(c, c.name_any(), "storage"),
        "spec": {
            "replicas": replicas,
            "serviceName": headless_name(c),
            // Nothing joins, nothing votes and nothing waits for a peer, so
            // ordered startup would only make an N-replica rollout N WAL
            // replays long.
            "podManagementPolicy": "Parallel",
            "selector": {"matchLabels": selector(c)},
            "template": {
                "metadata": {
                    "labels": storage_selector(c),
                    "annotations": {"mira.miradb.dev/config": config_hash(&node_config())},
                },
                "spec": {
                    "containers": [{
                        "name": "mira",
                        "image": c.spec.image,
                        "args": ["--config", "/etc/mira/mira.yaml"],
                        "env": [{
                            "name": "POD_NAME",
                            "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}},
                        }],
                        "ports": [
                            {"name": "otlp-grpc", "containerPort": GRPC},
                            {"name": "otlp-http", "containerPort": HTTP},
                        ],
                        // The probe that has to run before the other two, and
                        // the reason a replica with a large log comes back at
                        // all. `main.rs` binds the HTTP socket before it
                        // replays the WAL and serves after, so during a replay
                        // the port accepts and then answers nothing — every
                        // probe against it hangs to its own timeout. Liveness
                        // alone kills the pod about thirty seconds in, the
                        // restart replays the same log and dies at the same
                        // point, and the replica never returns. A startup probe
                        // suspends both of the others until the container has
                        // answered once. 60 x 5s = five minutes of replay.
                        "startupProbe": {
                            "httpGet": {"path": "/health", "port": HTTP},
                            "periodSeconds": 5,
                            "failureThreshold": 60,
                        },
                        // `/readyz` and not `/health` for readiness: the engine
                        // separates them, and the proxy must not be sent to a
                        // replica that is still replaying its WAL.
                        "readinessProbe": {
                            "httpGet": {"path": "/readyz", "port": HTTP},
                            "periodSeconds": 5,
                        },
                        "livenessProbe": {
                            "httpGet": {"path": "/health", "port": HTTP},
                            "periodSeconds": 10,
                        },
                        "resources": c.spec.resources,
                        "securityContext": container_security_context(),
                        "volumeMounts": [
                            {"name": "data", "mountPath": "/data"},
                            {"name": "config", "mountPath": "/etc/mira"},
                        ],
                    }],
                    "securityContext": pod_security_context(),
                    "automountServiceAccountToken": false,
                    // On SIGTERM the engine stops accepting, finishes the
                    // exports in flight and seals each signal's open block. The
                    // kubelet's default 30s is not enough for a loaded node to
                    // do that, and being killed part-way through is exactly the
                    // SIGKILL case that loses unflushed records — reached here
                    // on an ordinary rollout rather than only on a scale-in.
                    "terminationGracePeriodSeconds": 60,
                    "volumes": [{
                        "name": "config",
                        "configMap": {"name": c.name_any()},
                    }],
                },
            },
            // One RWO volume per replica. Mira mmaps its blocks and refuses to
            // start on a network filesystem, which rules out the single shared
            // RWX claim a Deployment would need — and ordinal identity is the
            // honest model anyway, because nothing replicates and nothing
            // rebalances, so replica 2's blocks are replica 2's.
            "volumeClaimTemplates": [{
                "metadata": {"name": "data"},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "resources": {"requests": {"storage": c.spec.storage.size}},
                    "storageClassName": c.spec.storage.class_name,
                },
            }],
        },
    }))
    .expect("statefulset is well-formed")
}

pub fn proxy_deployment(c: &MiraCluster, replicas: i32) -> Deployment {
    let cfg = proxy_config(c, replicas);
    serde_json::from_value(json!({
        "metadata": meta(c, proxy_name(c), "proxy"),
        "spec": {
            "replicas": c.spec.proxy.replicas,
            "selector": {"matchLabels": proxy_selector(c)},
            "template": {
                "metadata": {
                    "labels": proxy_selector(c),
                    // The line that makes a scale event reach the proxy. The
                    // engine reads the replica list once at boot, so a rewritten
                    // ConfigMap alone changes nothing until something restarts
                    // the pods; this annotation is that something.
                    "annotations": {"mira.miradb.dev/config": config_hash(&cfg)},
                },
                "spec": {
                    "containers": [{
                        "name": "proxy",
                        "image": c.spec.image,
                        "args": ["proxy", "--config", "/etc/mira/mira.yaml"],
                        "ports": [{"name": "otlp-http", "containerPort": HTTP}],
                        "readinessProbe": {
                            "httpGet": {"path": "/readyz", "port": HTTP},
                            "periodSeconds": 5,
                        },
                        "resources": c.spec.proxy.resources,
                        "securityContext": container_security_context(),
                        "volumeMounts": [{"name": "config", "mountPath": "/etc/mira"}],
                    }],
                    "securityContext": pod_security_context(),
                    "automountServiceAccountToken": false,
                    "volumes": [{
                        "name": "config",
                        "configMap": {"name": proxy_name(c)},
                    }],
                },
            },
        },
    }))
    .expect("proxy deployment is well-formed")
}

/// The Job that copies a replica's blocks out before its volume is deleted.
///
/// `mira offload push` and not a `preStop` hook, which is what the first design
/// of this reached for and had to drop: a pod cannot tell a scale-in from a
/// rolling restart, so a hook on `preStop` would evacuate every replica on the
/// next image bump. A Job created by the controller happens exactly when the
/// controller decided to shrink and at no other time.
///
/// It mounts the *existing* PVC by name. That is the whole trick — the StatefulSet
/// has already been scaled down, the pod is gone, and the claim outlives it,
/// which is the default `retentionPolicy` behaviour this relies on rather than
/// fights.
pub fn drain_job(c: &MiraCluster, ordinal: i32, offload: &str) -> Job {
    let name = format!("{}-drain-{}", c.name_any(), ordinal);
    let pod = format!("{}-{}", c.name_any(), ordinal);

    // `${node}` expanded here, because the engine will not expand it there.
    // Interpolation is a feature of the config *parser*, and `--offload` on the
    // command line is stored raw — so a Job handed the spec's string verbatim
    // writes the archive into a directory literally named `${node}`, one level
    // under the mount, and `mira offload restore` looks for `/cold/tel-2`. The
    // Job exits 0 either way and the claim is deleted either way.
    let offload = &offload.replace("${node}", &pod);

    // The cold store, mounted where the offload URL points. Without it `mira
    // offload push` writes into the container's own filesystem, exits 0, and
    // the operator deletes the claim it believes it has archived — so
    // `validate` refuses the spec that would produce `None` here, and the
    // `None` arm below is the unreachable half of a check that already ran
    // rather than a second policy.
    let cold = crate::crd::cold_mount(offload).zip(c.spec.cold_storage_claim.clone());
    let (mounts, volumes) = match &cold {
        Some((at, claim)) => (
            json!([
                {"name": "data", "mountPath": "/data"},
                {"name": "cold", "mountPath": at},
            ]),
            json!([
                {"name": "data", "persistentVolumeClaim": {
                    "claimName": format!("data-{}-{}", c.name_any(), ordinal)}},
                {"name": "cold", "persistentVolumeClaim": {"claimName": claim}},
            ]),
        ),
        None => (
            json!([{"name": "data", "mountPath": "/data"}]),
            json!([{"name": "data", "persistentVolumeClaim": {
                "claimName": format!("data-{}-{}", c.name_any(), ordinal)}}]),
        ),
    };

    serde_json::from_value(json!({
        "metadata": meta(c, name, "drain"),
        "spec": {
            // A drain that fails is not retried into a different shape — it
            // fails, the phase goes Degraded and the volume is still there.
            // Retrying forever would hide a full archive behind a Job that
            // looks busy.
            "backoffLimit": 3,
            // The other half of that, and the one `backoffLimit` does not
            // cover: it bounds *attempts*, not how long one runs. A copy
            // blocked on an unresponsive cold store never fails and never
            // finishes, so the reconcile requeues every ten seconds against a
            // tier that is one replica down and reports `Draining` for ever.
            // Past the deadline Kubernetes fails the Job itself, which is the
            // existing `Degraded` path with the volume kept.
            "activeDeadlineSeconds": c.spec.scaling.drain_deadline_seconds,
            "template": {
                "metadata": {"labels": labels(c, "drain")},
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "drain",
                        "image": c.spec.image,
                        // `push` copies and unlinks nothing. The volume it runs
                        // against is about to be deleted, so freeing space on
                        // it buys nothing, and an emptied directory walks two
                        // derived numbers backwards — see docs/architecture/retention.md 6.1.
                        "args": [
                            "offload", "push",
                            "--data-dir", "/data",
                            "--offload", offload,
                        ],
                        "env": [{"name": "POD_NAME", "value": pod}],
                        // The replica's sizing, because this is the replica's
                        // binary against the replica's volume. BestEffort here
                        // is the first thing evicted under node pressure, and
                        // the moment it would happen in is the copy that is the
                        // only surviving reference to a volume about to go.
                        "resources": c.spec.resources,
                        "securityContext": container_security_context(),
                        "volumeMounts": mounts,
                    }],
                    "securityContext": pod_security_context(),
                    "automountServiceAccountToken": false,
                    "volumes": volumes,
                },
            },
        },
    }))
    .expect("drain job is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{MiraClusterSpec, ParentRef, Proxy, Scaling, Storage};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    fn cluster() -> MiraCluster {
        let mut c = MiraCluster::new(
            "tel",
            MiraClusterSpec {
                image: "ghcr.io/trianalab/mira:0.0.4".into(),
                replicas: 1,
                max_replicas: 10,
                storage: Storage {
                    size: Quantity("10Gi".into()),
                    class_name: Some("fast".into()),
                },
                resources: None,
                scaling: Scaling::default(),
                offload: Some("file:///cold/${node}".into()),
                cold_storage_claim: Some("mira-cold".into()),
                proxy: Proxy::default(),
                route: None,
            },
        );
        c.metadata.namespace = Some("obs".into());
        c.metadata.uid = Some("uid-1".into());
        c
    }

    fn route() -> Route {
        Route {
            parent_refs: vec![ParentRef {
                name: "edge".into(),
                namespace: Some("gateways".into()),
                section_name: Some("http".into()),
            }],
            hostnames: vec!["mira.example.com".into()],
        }
    }

    /// The route is the one object here with no k8s-openapi type behind it, so
    /// nothing at all is checked at compile time: `sectionName` spelled
    /// `section_name`, `backendRefs` spelled `backendRef`, a port as a string —
    /// every one of those applies cleanly and routes nothing, because the
    /// apiserver prunes what the Gateway API schema does not describe.
    #[test]
    fn the_route_is_the_gateway_api_spelling() {
        let r = http_route(&cluster(), &route());
        let t = r.types.expect("typeMeta");
        assert_eq!(t.api_version, "gateway.networking.k8s.io/v1");
        assert_eq!(t.kind, "HTTPRoute");
        assert_eq!(r.metadata.name.as_deref(), Some("tel"));
        assert_eq!(r.metadata.namespace.as_deref(), Some("obs"));

        let spec = &r.data["spec"];
        assert_eq!(spec["hostnames"][0], "mira.example.com");
        assert_eq!(spec["parentRefs"][0]["name"], "edge");
        assert_eq!(spec["parentRefs"][0]["namespace"], "gateways");
        assert_eq!(spec["parentRefs"][0]["sectionName"], "http");
    }

    /// Ingest and the merged read go to the proxy; everything else goes to a
    /// storage node.
    ///
    /// Not a preference. The proxy answers `/v1/*` and `/api/v1/query` and
    /// named-501s the rest, and the UI, `/mcp` and the block-walking reads live
    /// only on a node — so a route that sent the catch-all to the proxy would
    /// serve a browser nothing but 501s.
    #[test]
    fn the_route_sends_ingest_to_the_proxy_and_the_rest_to_a_node() {
        let rules = http_route(&cluster(), &route()).data["spec"]["rules"].clone();
        assert_eq!(rules[0]["backendRefs"][0]["name"], "tel-proxy");
        assert_eq!(rules[0]["backendRefs"][0]["port"], HTTP);
        assert_eq!(rules[0]["matches"][0]["path"]["value"], "/v1/");
        assert_eq!(rules[0]["matches"][1]["path"]["type"], "Exact");
        assert_eq!(rules[0]["matches"][1]["path"]["value"], "/api/v1/query");

        assert_eq!(rules[1]["matches"][0]["path"]["value"], "/");
        assert_eq!(rules[1]["backendRefs"][0]["name"], "tel-headless");
    }

    /// `proxy.replicas: 0` is a supported tier, and a route that still named
    /// the proxy would send every ingest to a Service with no endpoints.
    #[test]
    fn a_tier_with_no_proxy_routes_everything_to_the_nodes() {
        let mut c = cluster();
        c.spec.proxy.replicas = 0;
        let rules = http_route(&c, &route()).data["spec"]["rules"].clone();
        assert_eq!(rules.as_array().expect("rules").len(), 1);
        assert_eq!(rules[0]["backendRefs"][0]["name"], "tel-headless");
    }

    /// No `hostnames` key rather than an empty list: an `HTTPRoute` with
    /// `hostnames: []` matches no hostname at all, where an absent one matches
    /// every hostname the listener accepts.
    #[test]
    fn no_hostnames_is_absent_rather_than_empty() {
        let c = cluster();
        let r = Route {
            hostnames: vec![],
            ..route()
        };
        assert!(http_route(&c, &r).data["spec"]["hostnames"].is_null());
    }

    /// The fields whose loss would be *silent*. `json!` into a type that
    /// ignores unknown keys means a typo deserialises to `None` and the object
    /// applies cleanly — a StatefulSet with no `volumeClaimTemplates` starts
    /// fine and loses every block on the first reschedule.
    #[test]
    fn the_statefulset_keeps_the_fields_a_typo_would_drop() {
        let s = stateful_set(&cluster(), 3);
        let spec = s.spec.expect("spec");
        assert_eq!(spec.replicas, Some(3));
        assert_eq!(spec.service_name.as_deref(), Some("tel-headless"));

        let vct = spec.volume_claim_templates.expect("volumeClaimTemplates");
        assert_eq!(vct.len(), 1);
        let claim = vct[0].spec.as_ref().expect("claim spec");
        assert_eq!(
            claim.access_modes.as_deref(),
            Some(&["ReadWriteOnce".to_string()][..])
        );
        assert_eq!(claim.storage_class_name.as_deref(), Some("fast"));

        let pod = spec.template.spec.expect("pod spec");
        let ctr = &pod.containers[0];
        assert_eq!(ctr.image.as_deref(), Some("ghcr.io/trianalab/mira:0.0.4"));
        // The mount, and the probe that decides whether the proxy is sent here.
        let mounts = ctr.volume_mounts.as_ref().expect("mounts");
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/data" && m.name == "data")
        );
        let probe = ctr.readiness_probe.as_ref().expect("readinessProbe");
        assert_eq!(
            probe.http_get.as_ref().unwrap().path.as_deref(),
            Some("/readyz")
        );
    }

    /// Every object the operator creates has to be garbage-collected with the
    /// cluster, or deleting a `MiraCluster` leaves a proxy serving traffic.
    #[test]
    fn every_object_is_owned_by_the_cluster() {
        let c = cluster();
        let owners = [
            stateful_set(&c, 1).metadata.owner_references,
            proxy_deployment(&c, 1).metadata.owner_references,
            headless_service(&c).metadata.owner_references,
            proxy_service(&c).metadata.owner_references,
            config_map(&c).metadata.owner_references,
            proxy_config_map(&c, 1).metadata.owner_references,
            drain_job(&c, 2, "file:///cold").metadata.owner_references,
            pod_disruption_budget(&c).metadata.owner_references,
            http_route(&c, &route()).metadata.owner_references,
        ];
        for o in owners {
            let o = o.expect("ownerReferences");
            assert_eq!(o[0].uid, "uid-1");
            assert_eq!(o[0].controller, Some(true));
        }
    }

    /// The headless Service is the storage tier's address and must not answer
    /// for the proxy as well.
    ///
    /// It used to, because it selected on [`selector`] — the immutable subset,
    /// which every pod under the cluster carries. The per-pod names the
    /// operator and the proxy use (`tel-0.tel-headless`) come from the
    /// StatefulSet's `serviceName`, so they kept working and nothing noticed;
    /// what broke was resolving the *Service* name, where a client got the
    /// proxy every other connection. Both answer `/api/v1/query`, so the
    /// symptom was a read that silently alternated between merged and
    /// node-local results — found by pointing an `HTTPRoute` at it.
    #[test]
    fn the_headless_service_addresses_storage_and_not_the_proxy() {
        let c = cluster();
        let sel = headless_service(&c)
            .spec
            .expect("spec")
            .selector
            .expect("selector");

        assert_eq!(sel, storage_selector(&c));
        assert_ne!(
            sel.get("app.kubernetes.io/component"),
            proxy_deployment(&c, 1)
                .spec
                .expect("spec")
                .selector
                .match_labels
                .expect("matchLabels")
                .get("app.kubernetes.io/component"),
        );
    }

    /// The selector must not carry anything that changes on an upgrade. A
    /// StatefulSet's `spec.selector` is immutable, so a version label in it
    /// makes the first image bump require deleting the object by hand.
    #[test]
    fn the_selector_is_immutable_across_an_image_bump() {
        let mut a = cluster();
        let before = selector(&a);
        a.spec.image = "ghcr.io/trianalab/mira:9.9.9".into();
        assert_eq!(before, selector(&a));
        assert!(!before.contains_key("app.kubernetes.io/version"));
    }

    /// The replica list is what a scale event actually changes, and the proxy
    /// reads it once at boot — so the pod annotation has to move with it or the
    /// scale never reaches the read tier.
    #[test]
    fn growing_the_tier_rewrites_the_proxy_list_and_rolls_it() {
        let c = cluster();
        let (two, three) = (proxy_config(&c, 2), proxy_config(&c, 3));
        assert!(two.contains("tel-0.tel-headless.obs.svc") && two.contains("tel-1."));
        assert!(!two.contains("tel-2."), "a two-replica list named a third");
        assert!(three.contains("tel-2."));

        let roll = |n| {
            proxy_deployment(&c, n)
                .spec
                .unwrap()
                .template
                .metadata
                .unwrap()
                .annotations
                .unwrap()["mira.miradb.dev/config"]
                .clone()
        };
        assert_ne!(roll(2), roll(3), "the proxy would not restart on a scale");
    }

    /// The drain has to mount the volume of the replica being removed. An
    /// off-by-one here archives the wrong replica and then deletes the one that
    /// was never copied.
    #[test]
    fn the_drain_job_mounts_the_departing_replicas_claim() {
        let j = drain_job(&cluster(), 4, "file:///cold/${node}");
        let pod = j.spec.unwrap().template.spec.unwrap();
        let claim = pod.volumes.unwrap()[0]
            .persistent_volume_claim
            .as_ref()
            .unwrap()
            .claim_name
            .clone();
        assert_eq!(claim, "data-tel-4");

        let args = pod.containers[0].args.clone().unwrap();
        assert!(args.contains(&"push".to_string()), "{args:?}");
        // The verb that must never appear here: `push` copies, and the volume
        // is about to go, so nothing is gained by freeing space on it.
        assert!(!args.iter().any(|a| a.contains("restore")), "{args:?}");
    }

    /// The other half of the drain, and the one whose absence was silent: the
    /// archive needs somewhere to land. Without this mount `mira offload push`
    /// writes `/cold/tel-4` into the container's own filesystem, exits 0, and
    /// the operator deletes `data-tel-4` believing it is archived.
    #[test]
    fn the_drain_job_mounts_the_cold_store_at_the_offload_root() {
        let pod = drain_job(&cluster(), 4, "file:///cold/${node}")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();

        let cold = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "cold")
            .expect("no cold volume; the archive would go to the container filesystem");
        assert_eq!(
            cold.persistent_volume_claim.as_ref().unwrap().claim_name,
            "mira-cold"
        );

        let at = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "cold")
            .expect("cold volume declared but never mounted")
            .mount_path
            .clone();
        // `/cold`, not `/cold/${node}`: the per-replica directory is created by
        // `offload push` underneath the mount, and mounting the expanded path
        // would mean one claim per drain forever.
        assert_eq!(at, "/cold");
    }

    /// The arm `validate` is supposed to make unreachable, pinned anyway.
    ///
    /// A cluster with no `coldStorageClaim` cannot be built through the CRD —
    /// the pair is refused — but this function takes the spec, not the
    /// verdict, and the shape it produces if the check is ever bypassed
    /// decides whether a volume is deleted. It must mount `data` and nothing
    /// else: a `cold` mount with no claim behind it is the container
    /// filesystem, which is the loss the check exists to prevent, and a
    /// well-formed Job is what lets the drain fail loudly instead.
    #[test]
    fn a_cluster_with_no_cold_claim_still_renders_a_job_that_mounts_only_data() {
        let mut c = cluster();
        c.spec.offload = None;
        c.spec.cold_storage_claim = None;

        let pod = drain_job(&c, 4, "file:///cold/${node}")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();

        let names: Vec<_> = pod
            .volumes
            .unwrap()
            .iter()
            .map(|v| v.name.clone())
            .collect();
        assert_eq!(names, ["data"]);
        let mounts = pod.containers[0].volume_mounts.clone().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount_path, "/data");
    }

    /// `spec.offload` must never reach a running replica's config.
    ///
    /// On a node, `storage.offload` is a *retention* setting: the sweep copies
    /// each expiring block to the target and then unlinks the local one. The
    /// replicas mount `data` and `config`, so the target would be the
    /// container's writable layer — blocks off the claim and into a directory
    /// that dies with the pod, phase still `Ready`. It is the exact loss
    /// `coldStorageClaim` was added to make unrepresentable, one object over.
    #[test]
    fn the_replica_config_never_names_the_cold_store() {
        // `cluster()` sets `offload`, so this fails if the field is ever piped
        // back through rather than only reaching `drain_job`.
        let cfg = config_map(&cluster()).data.unwrap()["mira.yaml"].clone();
        assert!(
            !cfg.contains("offload") && !cfg.contains("/cold"),
            "the replica would archive to an unmounted path and unlink the \
             original: {cfg}"
        );
    }

    /// The drain has to archive where a `restore` will look. It writes through
    /// `--offload`, which is not parsed as config and so is stored exactly as
    /// typed — `${node}` included. Left alone, the scale-in archive lands in a
    /// directory literally named `${node}` that nothing will ever read, and the
    /// claim is deleted all the same.
    #[test]
    fn the_drain_writes_where_a_restore_will_look() {
        let pod = drain_job(&cluster(), 4, "file:///cold/${node}")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        let args = pod.containers[0].args.clone().unwrap();
        assert!(
            args.contains(&"file:///cold/tel-4".to_string()),
            "the drain would archive to a literal ${{node}}: {args:?}"
        );
        // Same `POD_NAME` the replica ran under. The node name is hashed into
        // the block directory's name, so a drain under any other name walks a
        // directory it does not recognise and pushes nothing.
        let env = pod.containers[0].env.clone().unwrap();
        assert_eq!(env[0].value.as_deref(), Some("tel-4"));
    }

    /// A drain with no deadline is a scale-in that never ends. `backoffLimit`
    /// bounds *retries*, not the run: a copy blocked on an unresponsive cold
    /// store stays `active` for ever, the reconcile requeues every ten seconds
    /// for ever, and the phase reads `Draining` the whole time with nothing
    /// anywhere recording how long it has been doing that.
    #[test]
    fn a_drain_job_has_a_deadline_so_a_hung_copy_cannot_run_for_ever() {
        let spec = drain_job(&cluster(), 4, "file:///cold").spec.unwrap();
        assert_eq!(spec.active_deadline_seconds, Some(3600));

        let mut c = cluster();
        c.spec.scaling.drain_deadline_seconds = 90;
        let spec = drain_job(&c, 4, "file:///cold").spec.unwrap();
        assert_eq!(spec.active_deadline_seconds, Some(90));
    }

    /// Without a budget, `kubectl drain` on a node running two replicas evicts
    /// both, and each one's blocks are the only copy there is — nothing
    /// replicates and nothing rebalances, so two evictions is two holes in the
    /// corpus for as long as the volumes take to rebind.
    ///
    /// `maxUnavailable` and not `minAvailable`, which reads more naturally and
    /// is wrong here: `minAvailable: 1` on the single-replica tier that is the
    /// CRD's default allows no disruption at all, and a node drain against it
    /// blocks for ever on a pod the eviction API will never release.
    #[test]
    fn a_node_drain_may_take_one_replica_at_a_time_and_not_the_tier() {
        let b = pod_disruption_budget(&cluster());
        let spec = b.spec.expect("spec");
        assert_eq!(spec.max_unavailable, Some(IntOrString::Int(1)));
        assert_eq!(spec.min_available, None);

        // The storage pods, and only those. `matchLabels` is a subset match, so
        // the StatefulSet's own selector — which every pod the cluster owns
        // carries — made one budget of one cover the proxy's pods and the drain
        // Job's too: a node drain refused because a stateless proxy replica was
        // restarting, or because a Job the operator created was still copying.
        let pdb = spec
            .selector
            .expect("selector")
            .match_labels
            .expect("labels");
        let c = cluster();
        assert_eq!(pdb, storage_selector(&c));
        for other in [proxy_selector(&c), labels(&c, "drain")] {
            assert!(
                !pdb.iter().all(|(k, v)| other.get(k) == Some(v)),
                "the budget also selects {other:?}"
            );
        }
    }

    /// Every pod spec this module builds, named for the assertion message.
    fn pods(c: &MiraCluster) -> Vec<(&'static str, k8s_openapi::api::core::v1::PodSpec)> {
        let spec =
            |p: Option<k8s_openapi::api::core::v1::PodTemplateSpec>| p.unwrap().spec.unwrap();
        vec![
            (
                "statefulset",
                spec(Some(stateful_set(c, 1).spec.unwrap().template)),
            ),
            (
                "proxy",
                spec(Some(proxy_deployment(c, 1).spec.unwrap().template)),
            ),
            (
                "drain",
                spec(Some(
                    drain_job(c, 4, "file:///cold/${node}")
                        .spec
                        .unwrap()
                        .template,
                )),
            ),
        ]
    }

    /// A replica serves nothing until its log is replayed, and the liveness
    /// probe does not know that.
    ///
    /// `main.rs` binds the HTTP socket *before* it replays and calls
    /// `axum::serve` after, so during a replay the port accepts a connection
    /// and then answers nothing — the probe hangs until its own timeout rather
    /// than being refused, which is a failure and not an error. With liveness
    /// alone the kubelet is already counting: three failures at ten seconds
    /// kills the pod about thirty seconds in, the restart replays the same log
    /// and is killed at the same point, and the replica never comes back. A
    /// `startupProbe` is the only thing that holds liveness *and* readiness off
    /// until the container has answered once.
    #[test]
    fn a_replica_replaying_its_log_is_not_killed_for_not_answering_yet() {
        let s = stateful_set(&cluster(), 1);
        let ctr = &s.spec.unwrap().template.spec.unwrap().containers[0];

        let startup = ctr.startup_probe.as_ref().expect("startupProbe");
        assert_eq!(
            startup.http_get.as_ref().unwrap().path.as_deref(),
            Some("/health")
        );
        // The budget and not just the field: a startup probe that gives up
        // sooner than a replay takes is the same crash loop with more steps.
        let budget = startup.period_seconds.unwrap() * startup.failure_threshold.unwrap();
        assert!(budget >= 300, "{budget}s is not long enough for a replay");
    }

    /// A freshly provisioned PVC is mounted root-owned and the image runs as
    /// uid 65532.
    ///
    /// Without `fsGroup` the CSI driver never chowns it, so the first thing a
    /// new replica does — create a block directory — is a permission error;
    /// `mira_core::error` names this exact cause in the message the operator
    /// will read. `runAsUser` has to be spelled out beside `runAsNonRoot`
    /// because the Dockerfile's `USER nonroot:nonroot` is a *name*: the kubelet
    /// cannot prove a name is not root, and refuses to start the container at
    /// all if it is asked to.
    #[test]
    fn every_pod_here_can_write_the_volume_it_is_given() {
        for (what, pod) in pods(&cluster()) {
            let sc = pod
                .security_context
                .unwrap_or_else(|| panic!("{what}: no pod securityContext"));
            assert_eq!(sc.fs_group, Some(65532), "{what}: fsGroup");
            assert_eq!(sc.run_as_user, Some(65532), "{what}: runAsUser");
            assert_eq!(sc.run_as_non_root, Some(true), "{what}: runAsNonRoot");
            assert_eq!(
                sc.seccomp_profile.map(|p| p.type_),
                Some("RuntimeDefault".into()),
                "{what}: seccompProfile"
            );
            assert_eq!(
                pod.containers[0]
                    .security_context
                    .as_ref()
                    .and_then(|s| s.allow_privilege_escalation),
                Some(false),
                "{what}: allowPrivilegeEscalation"
            );
        }
    }

    /// Mira never calls the API server, so a mounted token is blast radius and
    /// nothing else — the difference between a compromised ingest process and a
    /// compromised cluster client. The operator's own chart already says the
    /// engine's pods turn it off, which stopped being true when the chart that
    /// did it was deleted.
    #[test]
    fn no_pod_here_is_handed_an_api_token_it_never_uses() {
        for (what, pod) in pods(&cluster()) {
            assert_eq!(pod.automount_service_account_token, Some(false), "{what}");
        }
    }

    /// Unset, every container here is BestEffort — the first thing the kubelet
    /// evicts under node pressure, and for the drain that is an eviction during
    /// the copy that is the only surviving reference to a volume about to be
    /// deleted. `docs/config.md` has carried a sizing table the whole time and a
    /// `MiraCluster` could not express one row of it.
    ///
    /// The drain reads `spec.resources` and not a field of its own: same binary,
    /// same volume, and a drain sized differently from the replica that filled
    /// it is a number with nothing to derive it from.
    #[test]
    fn the_sizes_a_spec_asks_for_reach_the_containers_that_honour_them() {
        for (what, pod) in pods(&cluster()) {
            assert_eq!(pod.containers[0].resources, None, "{what} defaults sized");
        }

        let sized = |cpu: &str| {
            Some(k8s_openapi::api::core::v1::ResourceRequirements {
                requests: Some(BTreeMap::from([("cpu".into(), Quantity(cpu.into()))])),
                ..Default::default()
            })
        };
        let mut c = cluster();
        c.spec.resources = sized("2500m");
        c.spec.proxy.resources = sized("500m");

        let cpu = |r: &Option<k8s_openapi::api::core::v1::ResourceRequirements>| {
            r.as_ref().unwrap().requests.as_ref().unwrap()["cpu"]
                .0
                .clone()
        };
        for (what, pod) in pods(&c) {
            let want = if what == "proxy" { "500m" } else { "2500m" };
            assert_eq!(cpu(&pod.containers[0].resources), want, "{what}");
        }
    }

    /// Killing a replica early turns a clean shutdown into a replay.
    ///
    /// On SIGTERM the engine stops accepting, finishes the exports in flight
    /// and seals each signal's open block. The kubelet's default is 30 seconds,
    /// which a loaded node does not finish in — and that is the SIGKILL case
    /// the scale-in ceiling names, reached on an ordinary rollout rather than
    /// on a scale-in. `main.rs` already tells the reader to raise this field,
    /// by name, in the error it prints when the port is still held.
    #[test]
    fn a_replica_is_given_long_enough_to_seal_what_it_is_holding() {
        let pod = stateful_set(&cluster(), 1)
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        assert_eq!(pod.termination_grace_period_seconds, Some(60));
    }
}
