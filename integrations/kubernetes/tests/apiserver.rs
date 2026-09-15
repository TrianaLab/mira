//! `reconcile()` against a real API server.
//!
//! The layer between the request-log tests in `controller.rs` and the Kind
//! suite in `e2e/`, and it exists because the two of them leave a gap that is
//! exactly the size of the API server's opinion.
//!
//! The request-log tests hand `Client::new` a `tower::Service` that replies
//! with whatever the test said to reply with. They prove the operator issues
//! the right requests in the right order, which is most of what a controller
//! is — and they cannot prove that any of those requests is *accepted*. A
//! StatefulSet with a `volumeClaimTemplates` entry whose `metadata.name` does
//! not match a `volumeMounts` name is a 422 from the API server and a cheerful
//! 200 from a fake. The Kind suite would catch it, twelve minutes and two
//! container builds later.
//!
//! There is no envtest for kube-rs — no crate that downloads a `kube-apiserver`
//! and `etcd` pair and hands you a kubeconfig, which is what `controller-
//! runtime` gives Go operators and half of pacto's controller tests are built
//! on. So this uses whatever cluster the current context points at, and skips
//! when told to run without one:
//!
//! ```sh
//! kind create cluster --name mira-apiserver-test
//! MIRA_OPERATOR_APISERVER=1 cargo test --manifest-path integrations/kubernetes/Cargo.toml \
//!   --test apiserver
//! ```
//!
//! Opt-in and not opt-out. `cargo test` has to stay cluster-free — it is the
//! command a contributor runs first, and a suite that fails on a laptop with no
//! kubeconfig teaches people to ignore it. `make operator-apiserver` sets the
//! variable, and CI runs it inside the Kind leg where the cluster already
//! exists.
//!
//! Each test gets a fresh namespace and deletes it on the way out, so they can
//! run in parallel against one cluster and a failure leaves one namespace
//! behind rather than poisoning the next run.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Namespace, ResourceRequirements, Service};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::runtime::wait::{await_condition, conditions};
use kube::{Api, Client, CustomResourceExt, ResourceExt};
use mira_operator::controller::{Ctx, reconcile};
use mira_operator::crd::{MiraCluster, MiraClusterSpec, Proxy, Scaling, Storage};
use serde_json::json;

/// The cluster, or `None` when this suite is not meant to run.
///
/// A missing kubeconfig with the variable set is a failure, not a skip — that
/// is CI having lost its cluster, and skipping there is how a gate goes green
/// for a month without running.
async fn client() -> Option<Client> {
    if std::env::var("MIRA_OPERATOR_APISERVER").is_err() {
        return None;
    }
    Some(
        Client::try_default()
            .await
            .expect("MIRA_OPERATOR_APISERVER is set but no cluster is reachable"),
    )
}

/// A namespace of this test's own, with the CRD installed cluster-wide.
///
/// The CRD is applied from `MiraCluster::crd()` — the same generator `crdgen`
/// writes the chart's copy from — rather than from the chart's YAML file. The
/// two agreeing is what `make operator-crd-check` is for; what this suite has
/// to test is that the *types* round-trip through apiextensions, and reading
/// the file would test the file instead.
async fn namespace(client: &Client, name: &str) -> Api<MiraCluster> {
    let crds: Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
        Api::all(client.clone());
    crds.patch(
        "miraclusters.mira.miradb.dev",
        &PatchParams::apply("mira-operator-test").force(),
        &Patch::Apply(MiraCluster::crd()),
    )
    .await
    .expect("the CRD did not apply");

    // Applied is not served. apiextensions installs the handler for
    // `/apis/mira.miradb.dev/v1alpha1/.../miraclusters` asynchronously, and
    // until it has, that path answers a bare `404 page not found` — not a
    // `Status` object, so the error reads "Failed to parse error data" and
    // looks like anything but a race. Five tests applying the same CRD in
    // parallel against a cluster created seconds earlier is what surfaces it.
    tokio::time::timeout(
        Duration::from_secs(30),
        await_condition(
            crds,
            "miraclusters.mira.miradb.dev",
            conditions::is_crd_established(),
        ),
    )
    .await
    .expect("the CRD was never established")
    .expect("watching the CRD failed");

    let nss: Api<Namespace> = Api::all(client.clone());
    let _ = nss.delete(name, &DeleteParams::default()).await;
    // A namespace terminates asynchronously and a create against a terminating
    // one is a 409, so wait it out rather than racing it.
    for _ in 0..60 {
        if nss.get_opt(name).await.expect("get namespace").is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    nss.create(
        &PostParams::default(),
        &serde_json::from_value(json!({"metadata": {"name": name}})).unwrap(),
    )
    .await
    .expect("the namespace did not create");

    Api::namespaced(client.clone(), name)
}

async fn drop_namespace(client: &Client, name: &str) {
    let nss: Api<Namespace> = Api::all(client.clone());
    let _ = nss.delete(name, &DeleteParams::default()).await;
}

/// Every field set, and no `..Default::default()`.
///
/// That omission is the test. A field added to `MiraClusterSpec` has to be
/// added here for this file to compile, and the round trip below then covers
/// it — whereas a struct-update fills new fields in silently and the assertion
/// keeps passing over a field nobody checked.
fn spec() -> MiraClusterSpec {
    MiraClusterSpec {
        image: "mira:test".into(),
        replicas: 2,
        max_replicas: 4,
        storage: Storage {
            size: Quantity("1Gi".into()),
            class_name: Some("standard".into()),
        },
        scaling: Scaling {
            up_when_free_below: 0.11,
            down_when_free_above: 0.77,
            cooldown_seconds: 42,
            drain_deadline_seconds: 900,
        },
        offload: Some("file:///cold/${node}".into()),
        cold_storage_claim: Some("mira-cold".into()),
        // Set rather than `None`, because this is the one field in the spec
        // whose schema a unit test cannot exercise: `ResourceRequirements`
        // generates `x-kubernetes-int-or-string` quantities, and only a real
        // apiserver decides whether a string in an int-or-string slot survives.
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity("2500m".into())),
                ("memory".into(), Quantity("2048Mi".into())),
            ])),
            ..Default::default()
        }),
        proxy: Proxy {
            replicas: 3,
            resources: Some(ResourceRequirements {
                limits: Some(BTreeMap::from([("cpu".into(), Quantity("1".into()))])),
                ..Default::default()
            }),
        },
    }
}

/// A CR created and read back, field for field.
///
/// The failure this exists for has no error message: apiextensions *prunes*
/// anything the schema does not describe. A field added to `MiraClusterSpec`
/// and not regenerated into the CRD is accepted by `kubectl apply`, returns
/// 201, and is simply gone by the time the reconciler reads the object back.
/// `make operator-crd-check` catches the chart's copy going stale; nothing
/// before this caught the schema itself being unable to carry a field —
/// `Option<String>` inside a flattened struct, say, or a map with no
/// `additionalProperties`.
#[tokio::test]
async fn every_spec_field_survives_a_round_trip_through_the_schema() {
    let Some(client) = client().await else { return };
    let ns = "mira-test-roundtrip";
    let api = namespace(&client, ns).await;

    let want = spec();
    api.create(
        &PostParams::default(),
        &MiraCluster::new("tel", want.clone()),
    )
    .await
    .expect("the API server rejected the CR");

    let got = api.get("tel").await.expect("get").spec;
    // Compared whole rather than field by field: a new field added to the spec
    // has to be added to `want` above for this to compile, which is the only
    // way this test keeps covering a struct it does not know the shape of.
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        serde_json::to_value(&want).unwrap(),
        "apiextensions pruned a field the CRD does not describe; run `make operator-crd`"
    );

    drop_namespace(&client, ns).await;
}

/// A ceiling below the floor never becomes an object.
///
/// `MiraClusterSpec::validate` refuses it too, and that is the wrong place for
/// it to be refused *first*: `kubectl apply` would return 201, the tier would
/// keep running whatever it was already running, and the only record that the
/// edit did nothing is a `Degraded` phase the person who typed it has to know
/// to go and read. The CEL rule moves the refusal into the apply.
///
/// Only a real apiserver can answer this. `x-kubernetes-validations` is
/// evaluated by apiextensions and by nothing in this crate, so a rule with a
/// typo in it — `self.maxReplicas` against a root schema where `self` is the
/// whole resource — generates, installs and silently never fires.
#[tokio::test]
async fn a_ceiling_below_the_floor_is_refused_by_the_apiserver() {
    let Some(client) = client().await else { return };
    let ns = "mira-test-ceiling";
    let api = namespace(&client, ns).await;

    let mut bad = spec();
    bad.replicas = 5;
    bad.max_replicas = 3;
    let e = api
        .create(&PostParams::default(), &MiraCluster::new("tel", bad))
        .await
        .expect_err("the API server accepted a ceiling below the floor")
        .to_string();
    assert!(e.contains("maxReplicas must be at least replicas"), "{e}");

    // And the floor itself, which is a plain `minimum` rather than a rule.
    let mut bad = spec();
    bad.replicas = 0;
    let e = api
        .create(&PostParams::default(), &MiraCluster::new("tel", bad))
        .await
        .expect_err("the API server accepted a tier of no replicas")
        .to_string();
    assert!(e.contains("replicas"), "{e}");

    drop_namespace(&client, ns).await;
}

/// One reconcile, and then the cluster is asked what it holds.
///
/// This is the request-log tests' assertion turned around. Those check the six
/// requests go out; this checks the six objects come back — which is the same
/// claim only if every one of those requests was accepted, and that is the part
/// a fake cannot answer.
#[tokio::test]
async fn a_reconcile_against_a_real_api_server_builds_the_whole_tier() {
    let Some(client) = client().await else { return };
    let ns = "mira-test-ensure";
    let api = namespace(&client, ns).await;

    let created = api
        .create(&PostParams::default(), &MiraCluster::new("tel", spec()))
        .await
        .expect("create");

    // The object as the API server returned it, with the uid it assigned —
    // every owner reference the operator writes needs one, and a locally built
    // fixture is the only place that has ever had to fake it.
    assert!(created.uid().is_some(), "no uid to own anything with");

    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });
    reconcile(Arc::new(created.clone()), ctx)
        .await
        .expect("reconcile returned an API error");

    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let set = sets.get("tel").await.expect("no StatefulSet");
    assert_eq!(set.spec.as_ref().unwrap().replicas, Some(2));

    // Server-side apply is the whole reason the field manager is a constant.
    // If this says anything else, a second reconcile will fight the first.
    let owned: Vec<_> = set
        .metadata
        .managed_fields
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.manager)
        .collect();
    assert!(
        owned.iter().any(|m| m == "mira-operator"),
        "the StatefulSet is not owned by the operator's field manager: {owned:?}"
    );

    // The owner reference, which is what makes `kubectl delete miracluster tel`
    // take the tier with it. Nothing else in the tree garbage-collects.
    let owner = set.metadata.owner_references.unwrap_or_default();
    assert_eq!(owner.len(), 1, "{owner:?}");
    assert_eq!(owner[0].uid, created.uid().unwrap());
    assert_eq!(owner[0].kind, "MiraCluster");

    let svcs: Api<Service> = Api::namespaced(client.clone(), ns);
    let headless = svcs.get("tel-headless").await.expect("no headless Service");
    assert_eq!(
        headless.spec.as_ref().unwrap().cluster_ip.as_deref(),
        Some("None"),
        "the headless Service was given a cluster IP; the replica DNS names will not resolve"
    );
    svcs.get("tel-proxy").await.expect("no proxy Service");

    drop_namespace(&client, ns).await;
}

/// Everything the operator is answerable for, and nothing anyone else writes.
///
/// `resourceVersion` was the obvious check and it is the wrong one on a real
/// cluster: kube-controller-manager writes the StatefulSet's `.status` within
/// milliseconds of the create, and that bumps the version with no second write
/// from the operator at all. It passed on a warm cluster and failed on the
/// freshly created one in the Kind suite, which is the whole point of running
/// this against a real server.
///
/// So: the spec, and this field manager's own `managedFields` entry — the set
/// of fields it owns and the last time it changed them. A server-side apply
/// that changes nothing is a no-op in etcd and leaves both untouched; one that
/// changes anything moves one or the other. Another manager's entry moving is
/// not this test's business.
fn operator_owned(set: &StatefulSet) -> serde_json::Value {
    let mine: Vec<_> = set
        .metadata
        .managed_fields
        .iter()
        .flatten()
        .filter(|f| f.manager.as_deref() == Some("mira-operator"))
        .collect();
    json!({
        "spec": set.spec,
        "generation": set.metadata.generation,
        "labels": set.metadata.labels,
        "annotations": set.metadata.annotations,
        "ownerReferences": set.metadata.owner_references,
        "managedFields": mine,
    })
}

/// Two reconciles in a row change nothing the second time.
///
/// A controller runs its reconcile on every watch event, including the ones its
/// own writes produce. If the second pass rewrites a field the first one set,
/// that is a write loop against the API server that no unit test sees — the
/// fake replies identically every time, so a fake cannot tell a converged
/// reconcile from an oscillating one.
#[tokio::test]
async fn a_second_reconcile_writes_nothing_new() {
    let Some(client) = client().await else { return };
    let ns = "mira-test-idempotent";
    let api = namespace(&client, ns).await;

    let c = Arc::new(
        api.create(&PostParams::default(), &MiraCluster::new("tel", spec()))
            .await
            .expect("create"),
    );
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });

    reconcile(c.clone(), ctx.clone()).await.expect("first");
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let first = sets.get("tel").await.expect("get");

    reconcile(c, ctx).await.expect("second");
    let second = sets.get("tel").await.expect("get");

    assert_eq!(
        operator_owned(&first),
        operator_owned(&second),
        "the second reconcile rewrote the StatefulSet; this is a write loop"
    );

    drop_namespace(&client, ns).await;
}

/// An invalid spec is reported on the CR and nothing is built.
///
/// The refusal added with `coldStorageClaim`, checked where it matters: the
/// status subresource is a separate endpoint with its own RBAC verb, and an
/// operator whose chart forgot `miraclusters/status` writes a perfect status
/// nowhere. The fake accepts the PATCH either way.
#[tokio::test]
async fn an_unsatisfiable_spec_lands_on_the_status_subresource() {
    let Some(client) = client().await else { return };
    let ns = "mira-test-degraded";
    let api = namespace(&client, ns).await;

    let mut bad = spec();
    // Offload with nowhere to write it: the drain would archive into the Job's
    // own container and the volume would be deleted regardless.
    bad.cold_storage_claim = None;

    let c = Arc::new(
        api.create(&PostParams::default(), &MiraCluster::new("tel", bad))
            .await
            .expect("create"),
    );
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });
    reconcile(c, ctx).await.expect("reconcile");

    let status = api
        .get("tel")
        .await
        .expect("get")
        .status
        .expect("no status");
    assert_eq!(status.phase.as_deref(), Some("Degraded"));
    assert!(
        status
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("coldStorageClaim"),
        "{status:?}"
    );

    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    assert!(
        sets.get_opt("tel").await.expect("get").is_none(),
        "a StatefulSet was built for a spec the operator had already refused"
    );

    drop_namespace(&client, ns).await;
}
