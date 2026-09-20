//! The reconcile loop.
//!
//! # The scale-in sequence, and why it is in this order
//!
//! Shrinking a Mira tier is not `replicas -= 1`. The volume about to go holds
//! blocks no other replica has — nothing replicates and nothing rebalances
//! (docs/architecture/replicas-scaling.md section 12.4) — so they have to be copied out first, and
//! the copy has to mount the volume.
//!
//! That mount is what fixes the order. The claim is `ReadWriteOnce`, so a Job
//! cannot attach it while the pod still holds it. The pod has to go first:
//!
//! 1. `status.draining = N-1`, written **before** anything is destroyed, so a
//!    controller that restarts mid-sequence resumes instead of leaving an
//!    orphaned claim nobody is looking at.
//! 2. Scale the StatefulSet to `N-1`. The pod goes; the claim does not, which
//!    is the default `persistentVolumeClaimRetentionPolicy` and the reason this
//!    works at all.
//! 3. Run `mira offload push` as a Job against the released claim.
//! 4. Only on success, delete the claim.
//! 5. Then delete the Job, which is what actually lets step 4 finish.
//!
//! Every step before 4 is reversible, and step 4 is gated on the archive
//! existing. A failed drain leaves the claim in place and the phase `Degraded`:
//! the tier is one replica smaller and the blocks are still on disk, which is a
//! bad day rather than a data-loss incident.
//!
//! Step 5 is not tidying up. `kubernetes.io/pvc-protection` keeps a claim alive
//! while any scheduled pod still references it, and a *completed* pod counts:
//! the Job's pod is not deleted when the Job finishes, so the claim deleted in
//! step 4 sits in `Terminating` for as long as the Job exists, which is for
//! ever. The operator would log "volume released" over a volume still on the
//! bill. Only on success, so a failed drain keeps the Job its own status message
//! tells an operator to go and read.
//!
//! # What a drain does not do
//!
//! It does not re-home the blocks onto a surviving replica. `offload push`
//! copies them to the cold store and they stay there, queryable only after a
//! deliberate `mira offload restore`. That is not an omission — section 12.4 is
//! "No rebalancing, ever", and the RWO claim that forced the order above would
//! force the same problem in reverse: a restore has to mount a *surviving*
//! replica's volume, which its own running pod holds. Automating it would mean
//! taking a healthy replica down to grow it.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams, PostParams,
};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use serde_json::json;
use tracing::{error, info, warn};

use crate::crd::MiraCluster;
use crate::resources as res;
use crate::stats::{self, Decision, Reading};

/// Field manager for server-side apply.
///
/// One name, used everywhere. Server-side apply and not `replace`: two
/// controllers and a human all editing a StatefulSet is the normal state of a
/// cluster, and `replace` resolves that by silently reverting whatever it did
/// not know about.
const MANAGER: &str = "mira-operator";

/// How long to wait for one replica's `/api/v1/stats`.
const STATS_TIMEOUT: Duration = Duration::from_secs(3);

/// Only the API's own failures. A spec this operator cannot satisfy is not one
/// of these: it becomes a `Degraded` status and a slow requeue, because
/// returning it as an error would put an unsatisfiable object into the
/// exponential-backoff path and retry it forever against the API server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
}

pub struct Ctx {
    pub client: Client,
}

fn apply<T: serde::Serialize>(v: &T) -> Patch<&T> {
    Patch::Apply(v)
}

fn params() -> PatchParams {
    // `force`: the operator is the owner of these fields, and a conflict here
    // means someone edited a managed object by hand. Losing that edit on the
    // next reconcile is the intended behaviour of a controller — the
    // alternative is a reconcile that wedges forever on a stale field manager.
    PatchParams::apply(MANAGER).force()
}

pub async fn reconcile(c: Arc<MiraCluster>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = c.namespace().unwrap_or_else(|| "default".into());
    let name = c.name_any();

    // Live, not the reflector cache `c` came from. `start_drain` writes the
    // status and *then* scales the StatefulSet, and the scale fires the
    // `.owns()` watch on a stream with no ordering against the MiraCluster
    // one — so on the reconcile that scale triggers, a cached `c.status` can
    // still read `draining: None`. The decision path below would then start a
    // second drain, overwrite `draining` with the next ordinal down, and leave
    // the first replica's claim with nothing anywhere recording that it
    // exists. `current` is read live for the same reason; this field is the
    // one where being stale costs a volume.
    //
    // The whole status is taken, not just that field, because `lastScaled` is
    // in the same window and being stale there costs the cooldown. `scale()`
    // fires the same watch before the status write recording it has even been
    // issued, so the reconcile it triggers reads no `lastScaled`, decides there
    // is no cooldown, and takes a branch that writes a status omitting it —
    // which under server-side apply *deletes* it. The tier is then free to
    // swing again on the next reading, creating and archiving a volume each
    // way. The spec and the metadata stay as delivered: those are what the
    // watch exists to hand over.
    let clusters: Api<MiraCluster> = Api::namespaced(ctx.client.clone(), &ns);
    let live = clusters.get_opt(&name).await?.and_then(|live| live.status);
    let mut owned = (*c).clone();
    owned.status = live;
    let c = &owned;
    let draining = c.status.as_ref().and_then(|s| s.draining);

    // Every status write below has to name this. Server-side apply removes
    // what this manager owned and then omitted, so a branch that leaves it out
    // is a branch that forgets the tier ever scaled.
    let carried = c.status.as_ref().and_then(|s| s.last_scaled.clone());

    // A drain already in flight owns the next transition, and it takes that
    // claim before anything else in this function touches the tier. Both of
    // the steps below used to run first, and both could abandon the volume:
    //
    //   * `ensure` re-applies the StatefulSet at `max(current, spec.replicas)`,
    //     so raising the floor mid-drain recreated the pod whose volume was
    //     being archived — and then `finish_drain` deleted the claim under it.
    //   * the Degraded patch names two fields, and server-side apply *removes*
    //     what a manager owned and then omits, so an invalid spec arriving
    //     mid-drain silently unset `draining` and freed the next pass to drain
    //     the following replica.
    //
    // Re-reading stats and deciding again here would in any case be deciding
    // from a tier that is mid-shrink.
    if let Some(ordinal) = draining {
        return finish_drain(&ctx, c, &ns, ordinal).await;
    }

    if let Err(e) = c.spec.validate() {
        // A spec that cannot be satisfied is reported and then left alone. Not
        // requeued fast: nothing the controller does will fix it, and a hot
        // loop on an invalid object is how an operator takes out an API server.
        //
        // `replicas` and `lastScaled` are repeated for the server-side-apply
        // reason spelled out at the end of `finish_drain`: omitting them here
        // resets the printer column to 0 and forgets the cooldown, so fixing a
        // typo in the spec would let the tier scale again immediately.
        status(
            &ctx,
            c,
            json!({
                "replicas": c.status.as_ref().map_or(c.spec.replicas, |s| s.replicas),
                "lastScaled": carried,
                "phase": "Degraded",
                "message": e,
            }),
        )
        .await?;
        warn!(%ns, %name, "invalid spec");
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    let sets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);
    let current = sets
        .get_opt(&name)
        .await?
        .and_then(|s| s.spec.and_then(|s| s.replicas))
        .unwrap_or(c.spec.replicas)
        .max(c.spec.replicas);

    // Everything that does not depend on the scale decision, first. A tier with
    // a broken Service is a tier whose stats cannot be read, so the decision
    // below would abstain forever if this ran after it.
    ensure(&ctx, c, &ns, current).await?;

    if let Some(remaining) = cooling_down(c) {
        return Ok(Action::requeue(remaining));
    }

    let readings = read_all(c, current).await;
    let lowest = readings
        .iter()
        .filter_map(|r| match r {
            Reading::Free(f) => Some(*f),
            _ => None,
        })
        .fold(f64::INFINITY, f64::min);
    let free = lowest.is_finite().then(|| format!("{lowest:.2}"));

    match stats::decide(
        &readings,
        c.spec.scaling.up_when_free_below,
        c.spec.scaling.down_when_free_above,
    ) {
        Decision::Up if current < c.spec.max_replicas => {
            info!(%ns, %name, from = current, "scaling out");
            scale(&sets, &name, current + 1).await?;
            status(
                &ctx,
                c,
                json!({
                    "replicas": current + 1,
                    "phase": "ScalingUp",
                    "freeFraction": free,
                    "lastScaled": now(),
                    "message": format!("free fell below {}", c.spec.scaling.up_when_free_below),
                }),
            )
            .await?;
        }
        Decision::Up => {
            // At the ceiling with the disk still filling. This is the state an
            // operator has to be loud about rather than retry quietly: nothing
            // it can do will free space, and the tier is heading for a full
            // volume.
            warn!(%ns, %name, max = c.spec.max_replicas, "at maxReplicas and still filling");
            status(
                &ctx,
                c,
                json!({
                    "replicas": current,
                    "phase": "Degraded",
                    "freeFraction": free,
                    "lastScaled": carried,
                    "message": format!(
                        "free is below {} but the tier is at maxReplicas ({})",
                        c.spec.scaling.up_when_free_below, c.spec.max_replicas
                    ),
                }),
            )
            .await?;
        }
        Decision::Down if current > c.spec.replicas => match c.spec.offload.as_deref() {
            Some(_) => return start_drain(&ctx, c, &ns, current, free).await,
            // The deliberate refusal. Scaling in without an archive deletes the
            // only copy of those blocks, and the cost of *not* shrinking is a
            // bill rather than the data.
            None => {
                status(
                    &ctx,
                    c,
                    json!({
                        "replicas": current,
                        "phase": "Ready",
                        "freeFraction": free,
                        "lastScaled": carried,
                        "message": "would scale in, but spec.offload is unset; \
                                    scaling in without an archive would delete the only copy",
                    }),
                )
                .await?;
            }
        },
        Decision::Down | Decision::Hold => {
            status(
                &ctx,
                c,
                json!({
                    "replicas": current,
                    "phase": "Ready",
                    "freeFraction": free,
                    "lastScaled": carried,
                    "message": null,
                }),
            )
            .await?;
        }
    }

    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Apply every object the cluster owns.
async fn ensure(ctx: &Ctx, c: &MiraCluster, ns: &str, replicas: i32) -> Result<(), Error> {
    let name = c.name_any();
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    let svcs: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let sets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), ns);
    let pdbs: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), ns);

    cms.patch(&name, &params(), &apply(&res::config_map(c)))
        .await?;
    svcs.patch(
        &res::headless_name(c),
        &params(),
        &apply(&res::headless_service(c)),
    )
    .await?;
    // `replicas` is passed through rather than read from the spec: on the way
    // back from a scale event the StatefulSet's own count is the truth, and
    // re-applying `spec.replicas` here would undo every scale-out on the next
    // reconcile.
    sets.patch(&name, &params(), &apply(&res::stateful_set(c, replicas)))
        .await?;
    // Unconditional, and for every tier including a single-replica one: the
    // budget is what stops a node drain evicting more of the tier at once than
    // it can afford, and a tier that has no proxy in front of it is not a tier
    // that can afford more.
    pdbs.patch(&name, &params(), &apply(&res::pod_disruption_budget(c)))
        .await?;

    ensure_proxy(ctx, c, ns, replicas).await?;
    ensure_route(ctx, c, ns).await
}

/// Apply the `HTTPRoute`, or take it away again.
///
/// Last, and after the Services it names as backends: a route applied before
/// them lands `ResolvedRefs: False` on the Gateway until the next reconcile,
/// which is a frightening thing to leave in a fresh cluster's events for no
/// reason.
///
/// Dynamically typed for the reason [`res::http_route`] gives — Gateway API is
/// a CRD, so there is no k8s-openapi type and no compile-time guarantee the
/// cluster serves the kind at all. A `spec.route` on a cluster without the
/// Gateway API CRDs is therefore a reconcile error rather than a silent
/// no-op, which is the honest direction: the object asked for something the
/// cluster cannot do.
async fn ensure_route(ctx: &Ctx, c: &MiraCluster, ns: &str) -> Result<(), Error> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "gateway.networking.k8s.io",
        "v1",
        "HTTPRoute",
    ));
    let routes: Api<DynamicObject> = Api::namespaced_with(ctx.client.clone(), ns, &ar);
    let name = c.name_any();

    let Some(route) = &c.spec.route else {
        // Removing `spec.route` closes the route. Leaving it standing would be
        // a tier that stays reachable after the field that exposed it was
        // deleted, and the owner reference only collects it when the whole
        // `MiraCluster` goes.
        //
        // The result is dropped on purpose and this is the one place it can be:
        // the overwhelmingly common case is a cluster that never asked for a
        // route and has no Gateway API CRDs, where this is a 404 on the
        // resource path rather than on an object. Neither that nor a genuine
        // absence is a reconcile failure.
        let _ = routes.delete(&name, &DeleteParams::default()).await;
        return Ok(());
    };
    routes
        .patch(&name, &params(), &apply(&res::http_route(c, route)))
        .await?;
    Ok(())
}

/// Apply the proxy's view of the tier: the replica list, and the Deployment
/// that rolls when it changes.
///
/// Split out of [`ensure`] because a drain has to narrow this and *only* this.
/// Calling `ensure` mid-drain would re-apply the StatefulSet at the pre-drain
/// count and recreate the pod being archived.
async fn ensure_proxy(ctx: &Ctx, c: &MiraCluster, ns: &str, replicas: i32) -> Result<(), Error> {
    if c.spec.proxy.replicas == 0 {
        return Ok(());
    }
    let name = res::proxy_name(c);
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    let svcs: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let deps: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);

    cms.patch(
        &name,
        &params(),
        &apply(&res::proxy_config_map(c, replicas)),
    )
    .await?;
    svcs.patch(&name, &params(), &apply(&res::proxy_service(c)))
        .await?;
    deps.patch(
        &name,
        &params(),
        &apply(&res::proxy_deployment(c, replicas)),
    )
    .await?;
    Ok(())
}

/// Read `/api/v1/stats` from every replica, concurrently.
async fn read_all(c: &MiraCluster, replicas: i32) -> Vec<Reading> {
    let reads = (0..replicas).map(|i| {
        let url = format!("http://{}:{}", res::replica_host(c, i), res::HTTP);
        async move { stats::read(&url, STATS_TIMEOUT).await }
    });
    futures::future::join_all(reads).await
}

/// Seconds left on the cooldown, or `None` if it has expired.
fn cooling_down(c: &MiraCluster) -> Option<Duration> {
    let last = c.status.as_ref()?.last_scaled.as_ref()?;
    let then = chrono_parse(last)?;
    let elapsed = now_secs().checked_sub(then)?;
    let window = c.spec.scaling.cooldown_seconds;
    (elapsed < window).then(|| Duration::from_secs((window - elapsed) as u64))
}

/// Begin a scale-in: record it, then remove the pod.
async fn start_drain(
    ctx: &Ctx,
    c: &MiraCluster,
    ns: &str,
    current: i32,
    free: Option<String>,
) -> Result<Action, Error> {
    let ordinal = current - 1;
    let name = c.name_any();

    // Written first, and this ordering is the crash-safety of the whole
    // sequence. If the process dies after the StatefulSet shrinks but before
    // this lands, the claim is orphaned and no reconcile ever looks at it
    // again.
    status(
        ctx,
        c,
        json!({
            "replicas": ordinal,
            "phase": "Draining",
            "freeFraction": free,
            "draining": ordinal,
            // Carried, not re-stamped, for the reason the failure branch of
            // `finish_drain` spells out — and carried at all because omitting a
            // field this manager owns is how server-side apply deletes it.
            // `finish_drain` stamps the new one when the shrink is actually
            // done.
            "lastScaled": c.status.as_ref().and_then(|s| s.last_scaled.clone()),
            "message": format!("draining replica {ordinal} before removing its volume"),
        }),
    )
    .await?;

    // Before the pod goes, not after. Every reconcile from here until the drain
    // ends returns at `finish_drain` without reaching `ensure`, so the replica
    // list the proxy holds at this moment is the one it holds for the whole
    // drain — minutes, or indefinitely if the archive fails. `fanout` is every
    // replica or none, so a list still naming the deleted pod is a tier that
    // answers no query and accepts no write until someone intervenes. Narrowing
    // first also means there is no instant where either the config being
    // replaced or the one replacing it names a replica that does not exist: the
    // pod is still up.
    ensure_proxy(ctx, c, ns, ordinal).await?;

    let sets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), ns);
    scale(&sets, &name, ordinal).await?;
    info!(%ns, %name, ordinal, "scaling in: pod removed, draining volume");

    // Requeue rather than create the Job here. The pod has to release the RWO
    // claim before anything else can mount it, and that is a kubelet operation
    // with no synchronous completion to await.
    Ok(Action::requeue(Duration::from_secs(15)))
}

/// Drive an in-flight drain to its end.
async fn finish_drain(ctx: &Ctx, c: &MiraCluster, ns: &str, ordinal: i32) -> Result<Action, Error> {
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    let job_name = format!("{}-drain-{}", c.name_any(), ordinal);

    let Some(job) = jobs.get_opt(&job_name).await? else {
        let offload = c.spec.offload.clone().unwrap_or_default();
        match jobs
            .create(
                &PostParams::default(),
                &res::drain_job(c, ordinal, &offload),
            )
            .await
        {
            Ok(_) => info!(%ns, ordinal, "drain job created"),
            // The claim is still attached to the departing pod. Not an error —
            // the detach is asynchronous and this is the expected first look.
            Err(e) => warn!(%ns, ordinal, "drain job not created yet: {e}"),
        }
        return Ok(Action::requeue(Duration::from_secs(15)));
    };

    let st = job.status.unwrap_or_default();
    if st.succeeded.unwrap_or(0) > 0 {
        // Only now. The claim is the last copy until the archive exists, and
        // this is the one irreversible step in the sequence.
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
        let claim = format!("data-{}-{}", c.name_any(), ordinal);
        // Retried, not warned past. Clearing `draining` below is what routes
        // every future reconcile away from this function, so a delete that
        // failed and was only logged is a delete that nothing ever attempts
        // again: the claim stays bound and billed for ever while the log line
        // under it says "volume released" and the status says "archived and
        // removed". Both would be false, and no state anywhere would let a
        // human or a later pass find it. A 429 from priority-and-fairness, a
        // policy webhook or a transient 500 are all recoverable, so hold
        // `draining` and come back. A 404 is the delete having already landed
        // on an earlier attempt, which is success.
        match pvcs.delete(&claim, &Default::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => {
                warn!(%ns, %claim, "archive complete but claim not deleted, retrying: {e}");
                return Ok(Action::requeue(Duration::from_secs(15)));
            }
        }
        info!(%ns, ordinal, "drain complete, volume released");
        // `replicas` is repeated rather than left to carry over, and this is the
        // one place server-side apply bites. A field this manager owned and then
        // omits is *removed*, not kept — so a patch that says only "Ready" also
        // silently unsets the count `start_drain` wrote, and the printer column
        // reads 0 on a tier that is running `ordinal` pods. Nothing reconciles
        // off it, which is exactly why it went unnoticed: the only consumer is
        // whoever is watching the scale-in happen.
        status(
            ctx,
            c,
            json!({
                "replicas": ordinal,
                "phase": "Ready",
                "draining": null,
                "lastScaled": now(),
                "message": format!("replica {ordinal} archived and removed"),
            }),
        )
        .await?;

        // Last, and after the status patch rather than before it. The Job is
        // how a reconcile interrupted mid-drain knows where it got to, so it
        // may only go once `draining` is cleared — delete it first and a crash
        // in between leaves a cluster that is still draining, with no Job, and
        // the next pass builds a second one against a claim that is already
        // going away. Background propagation because the pod is the point:
        // deleting a Job without it orphans exactly the object holding the
        // claim's finalizer.
        if let Err(e) = jobs.delete(&job_name, &DeleteParams::background()).await {
            warn!(%ns, %job_name, "drain job not deleted; its pod pins the claim: {e}");
        }
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    // The `Failed` condition first, and only then the failed-pod count. When
    // Kubernetes gives up on a Job it writes the condition and terminates the
    // pods afterwards, so there is a window where `status.failed` is still 0 —
    // and on a pod stuck terminating on an unreachable node that window has no
    // end. Counting only pods reads it as "still running": requeue every ten
    // seconds, phase `Draining`, for ever, over a drain that was abandoned by
    // the control plane. The condition also carries the *reason*, which is the
    // difference between sending an operator to a pod log and telling them the
    // log ends mid-copy because `drainDeadlineSeconds` was too short.
    let why = st
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "Failed" && c.status == "True")
        .map(|c| c.reason.clone().unwrap_or_else(|| "Failed".into()))
        .or_else(|| {
            let n = st.failed.unwrap_or(0);
            (n > 0).then(|| format!("{n} failed pods"))
        });

    if let Some(why) = why {
        // Stop here, loudly, with the claim intact. The tier is one replica
        // smaller and the blocks are still on disk.
        error!(%ns, ordinal, %why, "drain failed; volume kept");
        status(
            ctx,
            c,
            json!({
                "replicas": ordinal,
                // `draining` repeated, for the same server-side-apply reason as
                // `replicas` above and with a far worse failure than a wrong
                // printer column. This field is what routes the next reconcile
                // back into this function; omit it and SSA deletes it, the next
                // pass takes the decision path instead, and it is free to start
                // draining the *next* replica down while this one's claim and
                // its failed Job are still sitting there. Degraded has to stay
                // degraded — the way out is to fix the cause and delete the
                // Job, which makes the next pass build a new one and retry.
                "draining": ordinal,
                // Carried, not re-stamped. `now()` here would push the cooldown
                // forward on every 300s requeue and turn "when the tier last
                // changed size" into "when the operator last noticed this".
                "lastScaled": c.status.as_ref().and_then(|s| s.last_scaled.clone()),
                "phase": "Degraded",
                "message": format!(
                    "drain of replica {ordinal} failed ({why}); its volume was kept. \
                     Inspect job {job_name} — the blocks are intact on claim data-{}-{}",
                    c.name_any(), ordinal
                ),
            }),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    Ok(Action::requeue(Duration::from_secs(10)))
}

async fn scale(sets: &Api<StatefulSet>, name: &str, replicas: i32) -> Result<(), Error> {
    sets.patch_scale(
        name,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(json!({
            "apiVersion": "autoscaling/v1",
            "kind": "Scale",
            "spec": {"replicas": replicas},
        })),
    )
    .await?;
    Ok(())
}

async fn status(ctx: &Ctx, c: &MiraCluster, patch: serde_json::Value) -> Result<(), Error> {
    let api: Api<MiraCluster> =
        Api::namespaced(ctx.client.clone(), &c.namespace().unwrap_or_default());
    api.patch_status(
        &c.name_any(),
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(json!({
            "apiVersion": MiraCluster::api_version(&()),
            "kind": MiraCluster::kind(&()),
            "status": patch,
        })),
    )
    .await?;
    Ok(())
}

pub fn error_policy(_: Arc<MiraCluster>, e: &Error, _: Arc<Ctx>) -> Action {
    error!("reconcile failed: {e}");
    Action::requeue(Duration::from_secs(30))
}

/// Which namespaces to watch, from a comma-separated list, or `None` for the
/// whole cluster.
///
/// The chart sets this from `rbac.namespaces`, and the two have to agree. A
/// `Role` can only authorise a namespaced LIST, so an operator left on
/// `Api::all` under namespaced RBAC watches nothing at all: every reflector
/// takes a 403, backs off, retries, and the tier it was installed to manage is
/// never reconciled. Empty entries are dropped so a trailing comma is not a
/// watch on `""`, which the API server reads as every namespace.
pub(crate) fn namespaces(raw: Option<&str>) -> Option<Vec<String>> {
    let list: Vec<String> = raw?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    (!list.is_empty()).then_some(list)
}

/// An `Api` over one namespace, or over all of them.
///
/// Shared with `events.rs`, which has to scope its Event and pod watches to the
/// same namespaces for the same reason: a `Role` can only authorise a
/// namespaced LIST.
pub(crate) fn scoped<K>(client: Client, ns: Option<&str>) -> Api<K>
where
    K: Resource<Scope = kube::core::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match ns {
        Some(n) => Api::namespaced(client, n),
        None => Api::all(client),
    }
}

/// Run the controller until the process is asked to stop.
pub async fn run(client: Client) -> Result<(), kube::Error> {
    match namespaces(std::env::var("WATCH_NAMESPACES").ok().as_deref()) {
        None => watch(client, None).await,
        // One controller per namespace rather than one filtered stream:
        // kube-rs has no multi-namespace watcher, and a namespaced LIST is the
        // only kind a `Role` can authorise. They share the client, so this is
        // one connection pool and one reflector per namespace.
        Some(list) => {
            info!(namespaces = ?list, "watching only these namespaces");
            let each = list.into_iter().map(|ns| watch(client.clone(), Some(ns)));
            futures::future::join_all(each).await;
        }
    }
    Ok(())
}

async fn watch(client: Client, ns: Option<String>) {
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });
    let ns = ns.as_deref();

    Controller::new(scoped::<MiraCluster>(client.clone(), ns), Config::default())
        // Owned objects, so a StatefulSet somebody edited by hand is corrected
        // on the spot rather than at the next poll.
        .owns(scoped::<StatefulSet>(client.clone(), ns), Config::default())
        .owns(scoped::<Deployment>(client.clone(), ns), Config::default())
        .owns(scoped::<Job>(client, ns), Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                error!("controller: {e}");
            }
        })
        .await;
}

// Time, without a date library. The only two operations this file needs are
// "now, as RFC3339" and "how long since that string", and both are a handful of
// integer arithmetic on a Unix timestamp — cheaper than a dependency whose
// timezone database is irrelevant to a cooldown measured in minutes.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds as RFC3339 UTC.
pub(crate) fn now() -> String {
    fmt_rfc3339(now_secs())
}

pub(crate) fn fmt_rfc3339(secs: i64) -> String {
    // Days since the epoch to a civil date, by the standard algorithm. Kept
    // here rather than imported because it is the only calendar arithmetic the
    // operator does.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// RFC3339 UTC back to Unix seconds. `None` on anything it does not recognise,
/// which the caller treats as "no cooldown recorded".
pub(crate) fn chrono_parse(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let y2 = y - i64::from(m <= 2);
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + hh * 3_600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        MiraClusterSpec, MiraClusterStatus, ParentRef, Proxy, Route, Scaling, Storage,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    use crate::fake::{Fake, not_found};
    use http::StatusCode;

    impl Fake {
        /// The default control plane: every write succeeds, nothing exists yet.
        /// Nothing exists yet, including the MiraCluster's own live read — a
        /// 404 there is `get_opt` returning `None`, which is the same answer as
        /// "no drain in flight" and leaves every non-drain test reading exactly
        /// as it did before that read was added.
        fn ok() -> Self {
            Self::new(|method, path| match (method, path) {
                ("GET", _) => (StatusCode::NOT_FOUND, not_found()),
                (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
                _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
            })
        }

        /// As `ok()`, but the live read at the top of `reconcile` returns a
        /// status the fixture handed to `reconcile` does not carry.
        ///
        /// Served from here rather than from that fixture, because the two
        /// disagreeing is the entire reason the read is live: `start_drain`
        /// writes the status and then scales, the scale fires the `.owns()`
        /// watch on its own stream, and the reconcile that triggers can still
        /// be handed the cached object from before the write.
        fn live(status: MiraClusterStatus) -> Self {
            let mut c = drainable();
            c.status = Some(status);
            let doc = serde_json::to_value(&c).unwrap();
            Self::new(move |method, path| match (method, path) {
                ("GET", p) if p.ends_with("/miraclusters/tel") => (StatusCode::OK, doc.clone()),
                ("GET", _) => (StatusCode::NOT_FOUND, not_found()),
                (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
                _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
            })
        }

        /// A drain of `ordinal` in flight, visible only to that live read.
        fn mid_drain(ordinal: i32) -> Self {
            Self::live(MiraClusterStatus {
                draining: Some(ordinal),
                ..Default::default()
            })
        }

        fn ctx(&self) -> Arc<Ctx> {
            Arc::new(Ctx {
                client: self.client("ns"),
            })
        }
    }

    /// Enough of a `MiraCluster` to deserialize, for the replies to
    /// `patch_status`. Every other kind in this file has all-optional fields,
    /// so `{"metadata":{}}` is a valid one of those.
    fn cluster_doc() -> serde_json::Value {
        json!({
            "apiVersion": "mira.miradb.dev/v1alpha1",
            "kind": "MiraCluster",
            "metadata": {"name": "tel", "namespace": "ns"},
            "spec": {
                "image": "m:1", "replicas": 1, "maxReplicas": 5,
                "storage": {"size": "1Gi"},
            },
        })
    }

    /// A cluster the API-level tests can reconcile: namespaced, with an offload
    /// target, so the scale-in path is reachable.
    fn drainable() -> MiraCluster {
        let mut c = cluster(None, 600);
        c.meta_mut().namespace = Some("ns".into());
        // Every object the operator applies carries an owner reference, and
        // that needs a uid. The API server always assigns one, so only a
        // hand-built fixture has to remember.
        c.meta_mut().uid = Some("00000000-0000-0000-0000-000000000000".into());
        c.spec.offload = Some("file:///cold/${node}".into());
        // Required alongside `offload` — without it `validate` refuses the spec
        // and every reconcile below would assert on `Degraded` instead of on
        // the path it means to cover.
        c.spec.cold_storage_claim = Some("mira-cold".into());
        c
    }

    fn cluster(last: Option<&str>, cooldown: i64) -> MiraCluster {
        let mut c = MiraCluster::new(
            "tel",
            MiraClusterSpec {
                image: "m:1".into(),
                replicas: 1,
                max_replicas: 5,
                storage: Storage {
                    size: Quantity("1Gi".into()),
                    class_name: None,
                },
                resources: None,
                scaling: Scaling {
                    cooldown_seconds: cooldown,
                    ..Default::default()
                },
                offload: None,
                cold_storage_claim: None,
                proxy: Proxy::default(),
                route: None,
            },
        );
        c.status = Some(MiraClusterStatus {
            last_scaled: last.map(str::to_owned),
            ..Default::default()
        });
        c
    }

    /// The timestamp the cooldown is measured against is one the operator wrote
    /// itself, so the two halves have to agree. A round-trip that drifts by an
    /// hour is a cooldown that is off by an hour.
    #[test]
    fn a_timestamp_survives_being_written_and_read_back() {
        for t in [0, 1_000_000_000, 1_757_000_000, 2_000_000_000] {
            let s = fmt_rfc3339(t);
            assert_eq!(chrono_parse(&s), Some(t), "{s}");
        }
        assert_eq!(fmt_rfc3339(0), "1970-01-01T00:00:00Z");
        // A leap day, the case the civil-date algorithm exists to get right.
        assert_eq!(fmt_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    /// No recorded scale is not "cooling down forever" — a fresh cluster has to
    /// be allowed to act on its first reading.
    #[test]
    fn a_cluster_that_has_never_scaled_is_not_cooling_down() {
        assert!(cooling_down(&cluster(None, 600)).is_none());
        let mut c = cluster(None, 600);
        c.status = None;
        assert!(cooling_down(&c).is_none());
    }

    /// Garbage in the status field must not wedge scaling permanently. It is a
    /// field a human can edit.
    #[test]
    fn an_unparseable_timestamp_does_not_block_scaling() {
        assert!(cooling_down(&cluster(Some("yesterday"), 600)).is_none());
        assert!(cooling_down(&cluster(Some(""), 600)).is_none());
    }

    /// The window itself: a scale a moment ago holds, and one long past does
    /// not. Without this the operator acts again on a reading that predates its
    /// own last decision — before a new volume has even been bound.
    #[test]
    fn a_recent_scale_holds_and_an_old_one_does_not() {
        let recent = fmt_rfc3339(now_secs() - 10);
        let remaining = cooling_down(&cluster(Some(&recent), 600)).expect("still cooling");
        assert!(
            remaining.as_secs() > 500 && remaining.as_secs() <= 600,
            "{remaining:?}"
        );

        let old = fmt_rfc3339(now_secs() - 4_000);
        assert!(cooling_down(&cluster(Some(&old), 600)).is_none());
    }

    // -----------------------------------------------------------------------
    // Against an API server, or something shaped like one.
    //
    // Everything below drives a real `kube::Client` and asserts the requests it
    // put on the wire. The reconcile paths these cover are the ones that delete
    // things.
    // -----------------------------------------------------------------------

    /// Every object the tier owns, applied, before any scale decision is taken.
    ///
    /// The order is not incidental: a tier whose Service is missing is a tier
    /// whose stats cannot be read, so a reconcile that decided first would
    /// abstain forever on a cluster it had never finished creating.
    #[tokio::test]
    async fn ensure_applies_the_whole_tier_and_the_proxy_with_it() {
        let f = Fake::ok();
        let c = drainable();
        ensure(&f.ctx(), &c, "ns", 3).await.unwrap();

        assert_eq!(
            f.log(),
            [
                "PATCH /api/v1/namespaces/ns/configmaps/tel",
                "PATCH /api/v1/namespaces/ns/services/tel-headless",
                "PATCH /apis/apps/v1/namespaces/ns/statefulsets/tel",
                "PATCH /apis/policy/v1/namespaces/ns/poddisruptionbudgets/tel",
                "PATCH /api/v1/namespaces/ns/configmaps/tel-proxy",
                "PATCH /api/v1/namespaces/ns/services/tel-proxy",
                "PATCH /apis/apps/v1/namespaces/ns/deployments/tel-proxy",
                // No `spec.route`, so the route is taken away rather than left
                // standing. See `ensure_route`.
                "DELETE /apis/gateway.networking.k8s.io/v1/namespaces/ns/httproutes/tel",
            ]
        );

        // The count that is applied is the one passed in, not `spec.replicas`.
        // Re-asserting the spec here would undo every scale-out on the very
        // next reconcile — a bug that looks like the tier refusing to grow.
        let set = f.body("statefulsets/tel");
        assert_eq!(set["spec"]["replicas"], 3);
    }

    /// `proxy.replicas: 0` is a supported topology, not a degenerate one.
    #[tokio::test]
    async fn a_tier_with_no_proxy_applies_only_its_own_four_objects() {
        let f = Fake::ok();
        let mut c = drainable();
        c.spec.proxy.replicas = 0;
        ensure(&f.ctx(), &c, "ns", 1).await.unwrap();

        let applied: Vec<_> = f
            .log()
            .into_iter()
            .filter(|l| l.starts_with("PATCH"))
            .collect();
        assert_eq!(applied.len(), 4, "{applied:?}");
        assert!(!f.log().iter().any(|l| l.contains("proxy")));
    }

    /// `spec.route` is a toggle in both directions.
    ///
    /// The off arm is the one worth a test. An operator that only ever *created*
    /// the route would leave a tier reachable through a Gateway after the field
    /// that put it there was deleted, and the owner reference does not help:
    /// it collects the route when the whole `MiraCluster` goes, which is not
    /// what removing one field asked for.
    #[tokio::test]
    async fn the_route_follows_the_field_that_asks_for_it() {
        let route = "/apis/gateway.networking.k8s.io/v1/namespaces/ns/httproutes/tel";

        let f = Fake::ok();
        let mut c = drainable();
        c.spec.route = Some(Route {
            parent_refs: vec![ParentRef {
                name: "edge".into(),
                namespace: None,
                section_name: None,
            }],
            hostnames: vec![],
        });
        ensure(&f.ctx(), &c, "ns", 1).await.unwrap();
        assert!(f.log().contains(&format!("PATCH {route}")), "{:?}", f.log());

        let f = Fake::ok();
        c.spec.route = None;
        ensure(&f.ctx(), &c, "ns", 1).await.unwrap();
        assert!(
            f.log().contains(&format!("DELETE {route}")),
            "{:?}",
            f.log()
        );
    }

    /// A cluster with no Gateway API CRDs is the common case, and the delete
    /// above is then a 404 on a resource path the API server has never heard
    /// of. That must not fail a reconcile of a tier that is otherwise healthy
    /// and never asked for a route.
    #[tokio::test]
    async fn a_cluster_without_the_gateway_api_still_reconciles() {
        let f = Fake::new(|m, p| {
            if m == "DELETE" && p.contains("httproutes") {
                (StatusCode::NOT_FOUND, not_found())
            } else {
                (StatusCode::OK, json!({}))
            }
        });
        ensure(&f.ctx(), &drainable(), "ns", 1).await.unwrap();
    }

    /// An unsatisfiable spec is reported and then left alone. It must not reach
    /// the API server for anything else: a hot loop on an object no reconcile
    /// can fix is how an operator takes out a control plane.
    #[tokio::test]
    async fn an_oscillating_spec_is_reported_and_nothing_else_is_touched() {
        let f = Fake::ok();
        let mut c = drainable();
        c.spec.scaling.up_when_free_below = 0.6;
        c.spec.scaling.down_when_free_above = 0.2;

        let action = reconcile(Arc::new(c), f.ctx()).await.unwrap();

        assert_eq!(
            f.log(),
            [
                "GET /apis/mira.miradb.dev/v1alpha1/namespaces/ns/miraclusters/tel",
                "PATCH /apis/mira.miradb.dev/v1alpha1/namespaces/ns/miraclusters/tel/status",
            ]
        );
        let st = f.body("/status")["status"].clone();
        assert_eq!(st["phase"], "Degraded");
        // Server-side apply removes what this manager owned and then omits, so
        // the patch has to carry the fields it is not changing. Dropping
        // `lastScaled` would forget the cooldown, and fixing the typo in the
        // spec would then let the tier scale again on the very next pass.
        assert!(st.get("replicas").is_some(), "{st}");
        assert!(st.get("lastScaled").is_some(), "{st}");
        assert_eq!(action, Action::requeue(Duration::from_secs(300)));
    }

    /// A spec that goes invalid *under* an in-flight drain must not cancel it.
    ///
    /// The Degraded patch names its fields, and server-side apply removes the
    /// ones a manager owned and then omitted — so when this ran before the
    /// drain check, an edit that tripped `validate` silently unset `draining`.
    /// The next pass took the decision path instead, free to start draining the
    /// replica below while this one's claim and its half-run Job were still
    /// there, with nothing anywhere recording that the volume existed.
    #[tokio::test]
    async fn a_spec_that_goes_invalid_under_a_drain_does_not_cancel_it() {
        let f = Fake::mid_drain(2);
        let mut c = drainable();
        c.spec.scaling.up_when_free_below = 0.6;
        c.spec.scaling.down_when_free_above = 0.2;

        reconcile(Arc::new(c), f.ctx()).await.unwrap();

        assert!(
            f.log().iter().any(|l| l.contains("jobs/tel-drain-2")),
            "the drain owns the pass; Degraded must wait its turn: {:?}",
            f.log()
        );
        assert!(
            !f.log().iter().any(|l| l.contains("/status")),
            "{:?}",
            f.log()
        );
    }

    /// `spec.replicas` is a floor and `current` takes `max()` of it, so raising
    /// the floor takes effect immediately — including mid-drain. When `ensure`
    /// ran before the drain check, that re-applied the StatefulSet at the
    /// higher count and recreated the pod whose volume was being archived;
    /// `finish_drain` then deleted the claim out from under it, or wedged
    /// forever because the drain Job could not mount a claim the new pod held.
    #[tokio::test]
    async fn raising_the_floor_mid_drain_does_not_rebuild_the_pod_being_archived() {
        let f = Fake::mid_drain(2);
        let mut c = drainable();
        c.spec.replicas = 3;

        reconcile(Arc::new(c), f.ctx()).await.unwrap();

        assert!(
            !f.log().iter().any(|l| l.contains("statefulsets/tel")),
            "the StatefulSet must not be re-applied while a drain is in flight: {:?}",
            f.log()
        );
        assert!(f.log().iter().any(|l| l.contains("jobs/tel-drain-2")));
    }

    /// Step 1 before step 2, which is the crash-safety of the whole sequence.
    ///
    /// If the process dies between the two, the recorded `draining` ordinal is
    /// what the next reconcile resumes from. In the other order the StatefulSet
    /// has already shrunk, nothing records why, and the claim is orphaned with
    /// no reconcile ever looking at it again.
    #[tokio::test]
    async fn a_drain_records_itself_before_it_removes_the_pod() {
        let f = Fake::ok();
        let mut c = drainable();
        c.status.as_mut().unwrap().last_scaled = Some("2024-01-01T00:00:00Z".into());
        let action = start_drain(&f.ctx(), &c, "ns", 3, Some("0.90".into()))
            .await
            .unwrap();

        assert_eq!(
            f.log().first().map(String::as_str),
            Some("PATCH /apis/mira.miradb.dev/v1alpha1/namespaces/ns/miraclusters/tel/status"),
            "status must be written before anything is destroyed: {:?}",
            f.log()
        );
        assert_eq!(
            f.log().last().map(String::as_str),
            Some("PATCH /apis/apps/v1/namespaces/ns/statefulsets/tel/scale"),
            "the pod goes last: {:?}",
            f.log()
        );

        let st = f.body("/status")["status"].clone();
        assert_eq!(st["phase"], "Draining");
        assert_eq!(st["draining"], 2);
        // Carried. Server-side apply removes what this manager owned and then
        // omitted, and the shrink is not finished — `finish_drain` stamps the
        // new one.
        assert_eq!(st["lastScaled"], "2024-01-01T00:00:00Z");
        assert_eq!(f.body("/scale")["spec"]["replicas"], 2);
        assert_eq!(action, Action::requeue(Duration::from_secs(15)));
    }

    /// The proxy stops naming the replica before the replica goes.
    ///
    /// Every reconcile from here until the drain ends returns at `finish_drain`
    /// without reaching `ensure`, so the replica list the proxy holds at this
    /// moment is the one it holds for the whole drain. `fanout` is every
    /// replica or none, so a list still naming the deleted pod is a tier that
    /// answers no query and accepts no write — for as long as the archive takes,
    /// and for ever if it fails.
    #[tokio::test]
    async fn a_drain_narrows_the_proxy_before_the_pod_it_removes_goes() {
        let f = Fake::ok();
        start_drain(&f.ctx(), &drainable(), "ns", 3, None)
            .await
            .unwrap();

        assert_eq!(
            f.log(),
            [
                "PATCH /apis/mira.miradb.dev/v1alpha1/namespaces/ns/miraclusters/tel/status",
                "PATCH /api/v1/namespaces/ns/configmaps/tel-proxy",
                "PATCH /api/v1/namespaces/ns/services/tel-proxy",
                "PATCH /apis/apps/v1/namespaces/ns/deployments/tel-proxy",
                "PATCH /apis/apps/v1/namespaces/ns/statefulsets/tel/scale",
            ],
            "and nothing else: re-applying the StatefulSet here would undo the scale below it"
        );

        let list = f.body("configmaps/tel-proxy")["data"]["mira.yaml"].clone();
        let list = list.as_str().unwrap();
        assert!(list.contains("tel-1."), "{list}");
        assert!(
            !list.contains("tel-2."),
            "the departing replica is still routed to: {list}"
        );
    }

    /// A drain in flight owns the next transition. Re-reading stats and
    /// deciding again from a tier that is mid-shrink is how one decision moves
    /// two replicas.
    ///
    /// The fixture's *cached* status is deliberately empty and the drain is
    /// visible only to the live read. That is the case that orphaned a volume:
    /// `start_drain` scales the StatefulSet right after writing the status, the
    /// scale fires the `.owns()` watch on a stream with no ordering against the
    /// MiraCluster one, and the reconcile it triggers could see the status from
    /// before the drain.
    #[tokio::test]
    async fn a_reconcile_mid_drain_resumes_it_rather_than_deciding_again() {
        let f = Fake::mid_drain(2);
        let c = drainable();
        assert!(c.status.as_ref().and_then(|s| s.draining).is_none());

        reconcile(Arc::new(c), f.ctx()).await.unwrap();

        // The drain's own GET is there; no scale, and no status write deciding
        // anything, because `finish_drain` owns the object now.
        assert!(f.log().iter().any(|l| l.contains("jobs/tel-drain-2")));
        assert!(
            !f.log().iter().any(|l| l.contains("/scale")),
            "{:?}",
            f.log()
        );
    }

    /// The Job is created against a claim the departing pod may still hold.
    /// Nothing is deleted on this pass.
    #[tokio::test]
    async fn a_drain_with_no_job_yet_creates_one_and_deletes_nothing() {
        let f = Fake::ok();
        let c = drainable();
        let action = finish_drain(&f.ctx(), &c, "ns", 2).await.unwrap();

        assert_eq!(
            f.log(),
            [
                "GET /apis/batch/v1/namespaces/ns/jobs/tel-drain-2",
                "POST /apis/batch/v1/namespaces/ns/jobs",
            ]
        );
        assert_eq!(action, Action::requeue(Duration::from_secs(15)));
    }

    /// The one irreversible step, and the only condition under which it may
    /// run. Until the Job reports `succeeded`, that claim is the last copy of
    /// those blocks.
    #[tokio::test]
    async fn the_claim_is_deleted_only_after_the_archive_succeeds() {
        let f = Fake::new(|method, path| match (method, path) {
            ("GET", p) if p.contains("/jobs/") => (
                StatusCode::OK,
                json!({"metadata": {"name": "tel-drain-2"}, "status": {"succeeded": 1}}),
            ),
            (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
            _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
        });
        let c = drainable();
        finish_drain(&f.ctx(), &c, "ns", 2).await.unwrap();

        // The Job goes last, after the status patch. A completed pod holds the
        // claim's `pvc-protection` finalizer, so the DELETE two lines up does
        // not finish until this one runs; and it runs after the patch so that a
        // crash in the gap leaves a resumable drain rather than a cleared one
        // with no Job to resume from.
        assert_eq!(
            f.log(),
            [
                "GET /apis/batch/v1/namespaces/ns/jobs/tel-drain-2",
                "DELETE /api/v1/namespaces/ns/persistentvolumeclaims/data-tel-2",
                "PATCH /apis/mira.miradb.dev/v1alpha1/namespaces/ns/miraclusters/tel/status",
                "DELETE /apis/batch/v1/namespaces/ns/jobs/tel-drain-2",
            ]
        );
        let st = f.body("/status")["status"].clone();
        assert_eq!(st["phase"], "Ready");
        // Cleared, not left set, or every later reconcile resumes a finished
        // drain and the tier never scales again.
        assert!(st["draining"].is_null());
        // Repeated, not carried over. This patch is a server-side apply by the
        // same manager that wrote the count in `start_drain`, so a key left out
        // here is a key deleted there — the Kind suite caught `REPLICAS 0`
        // beside two running pods.
        assert_eq!(st["replicas"], 2);
    }

    /// The status patch two lines after the claim delete clears `draining`,
    /// and that is what routes every future reconcile away from `finish_drain`.
    /// So a delete that failed and was only logged is a delete nothing ever
    /// attempts again: the claim stays bound and billed while the status reads
    /// "archived and removed" and no state anywhere records otherwise.
    ///
    /// Both directions, because the retry is only safe if the second attempt
    /// can finish: a 429 holds the drain open, and the 404 it leaves behind
    /// once the delete does land is success rather than a permanent wedge.
    #[tokio::test]
    async fn a_claim_that_could_not_be_deleted_keeps_the_drain_open() {
        fn fake(code: StatusCode) -> Fake {
            Fake::new(move |method, path| match (method, path) {
                ("GET", p) if p.contains("/jobs/") => (
                    StatusCode::OK,
                    json!({"metadata": {"name": "tel-drain-2"}, "status": {"succeeded": 1}}),
                ),
                ("DELETE", p) if p.contains("/persistentvolumeclaims/") => (
                    code,
                    json!({"kind": "Status", "status": "Failure", "code": code.as_u16(),
                           "reason": "TooManyRequests", "message": "please try again"}),
                ),
                (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
                _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
            })
        }

        let f = fake(StatusCode::TOO_MANY_REQUESTS);
        let action = finish_drain(&f.ctx(), &drainable(), "ns", 2).await.unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(15)));
        assert_eq!(
            f.log(),
            [
                "GET /apis/batch/v1/namespaces/ns/jobs/tel-drain-2",
                "DELETE /api/v1/namespaces/ns/persistentvolumeclaims/data-tel-2",
            ],
            "`draining` must stay set, so neither the status patch nor the Job delete runs"
        );

        // Already gone is done. Anything else and a drain whose claim was
        // deleted by an earlier attempt could never be closed out.
        let f = fake(StatusCode::NOT_FOUND);
        finish_drain(&f.ctx(), &drainable(), "ns", 2).await.unwrap();
        assert!(
            f.log().iter().any(|l| l.contains("/status")),
            "{:?}",
            f.log()
        );
        assert!(f.body("/status")["status"]["draining"].is_null());
    }

    /// A failed drain is a bad day, not a data-loss incident: the tier is one
    /// replica smaller and every block is still on disk. The claim must survive.
    #[tokio::test]
    async fn a_failed_drain_keeps_the_volume() {
        let f = Fake::new(|method, path| match (method, path) {
            ("GET", p) if p.contains("/jobs/") => (
                StatusCode::OK,
                json!({"metadata": {"name": "tel-drain-2"}, "status": {"failed": 1}}),
            ),
            (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
            _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
        });
        let c = drainable();
        let action = finish_drain(&f.ctx(), &c, "ns", 2).await.unwrap();

        // Nothing at all is deleted, and the Job least of all: its pod holds
        // the log of why the archive failed, and the status message sends an
        // operator to read it.
        assert!(
            !f.log().iter().any(|l| l.starts_with("DELETE")),
            "a failed archive must never delete the claim or the evidence: {:?}",
            f.log()
        );
        let st = &f.body("/status")["status"];
        assert_eq!(st["phase"], "Degraded");
        // The one that matters. `draining` is what sends the next reconcile
        // back here rather than to the decision path, and this patch is an
        // apply: a key it does not name is a key the server deletes. Without
        // this line a failed drain un-wedges itself, and the pass after it can
        // pick the next replica down while this one's claim is still orphaned.
        assert_eq!(st["draining"], 2);
        assert_eq!(action, Action::requeue(Duration::from_secs(300)));
    }

    /// The deadline is only half the fix. When Kubernetes gives up on a Job it
    /// writes a `Failed` condition and *then* terminates the pods, so there is a
    /// window — unbounded, if a pod is stuck terminating on an unreachable node
    /// — where `status.failed` is still 0. Counting only failed pods reads that
    /// as "still running": requeue every ten seconds, phase `Draining`, for
    /// ever, over a drain the control plane has already abandoned.
    #[tokio::test]
    async fn a_drain_kubernetes_has_already_given_up_on_is_reported_as_failed() {
        let f = Fake::new(|method, path| match (method, path) {
            ("GET", p) if p.contains("/jobs/") => (
                StatusCode::OK,
                json!({"metadata": {"name": "tel-drain-2"}, "status": {"conditions": [{
                    "type": "Failed", "status": "True", "reason": "DeadlineExceeded",
                    "message": "Job was active longer than specified deadline",
                }]}}),
            ),
            (_, p) if p.ends_with("/status") => (StatusCode::OK, cluster_doc()),
            _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
        });
        let action = finish_drain(&f.ctx(), &drainable(), "ns", 2).await.unwrap();

        assert!(
            !f.log().iter().any(|l| l.starts_with("DELETE")),
            "{:?}",
            f.log()
        );
        let st = &f.body("/status")["status"];
        assert_eq!(st["phase"], "Degraded");
        assert_eq!(st["draining"], 2);
        // The reason, not just the fact. "failed" sends an operator to a pod
        // log; "DeadlineExceeded" tells them the log ends mid-copy and the
        // deadline is the field to raise.
        let msg = st["message"].as_str().unwrap();
        assert!(msg.contains("DeadlineExceeded"), "{msg}");
        assert_eq!(action, Action::requeue(Duration::from_secs(300)));
    }

    /// A Job still running is neither outcome. Nothing happens but a requeue.
    #[tokio::test]
    async fn a_running_drain_job_is_left_to_run() {
        let f = Fake::new(|method, path| match (method, path) {
            ("GET", p) if p.contains("/jobs/") => (
                StatusCode::OK,
                json!({"metadata": {"name": "tel-drain-2"}, "status": {"active": 1}}),
            ),
            _ => (StatusCode::OK, json!({"metadata": {"name": "tel"}})),
        });
        let action = finish_drain(&f.ctx(), &drainable(), "ns", 2).await.unwrap();

        assert_eq!(
            f.log(),
            ["GET /apis/batch/v1/namespaces/ns/jobs/tel-drain-2"]
        );
        assert_eq!(action, Action::requeue(Duration::from_secs(10)));
    }

    /// Unreachable replicas are the state every one of these tests runs in —
    /// there is no tier behind the fake — and the decision they must produce is
    /// "do nothing". A controller that read silence as "empty disk" would scale
    /// a partitioned tier out forever.
    #[tokio::test]
    async fn a_tier_that_cannot_be_read_is_held_rather_than_scaled() {
        let f = Fake::ok();
        let action = reconcile(Arc::new(drainable()), f.ctx()).await.unwrap();

        assert!(
            !f.log().iter().any(|l| l.contains("/scale")),
            "{:?}",
            f.log()
        );
        assert_eq!(f.body("/status")["status"]["phase"], "Ready");
        assert_eq!(action, Action::requeue(Duration::from_secs(60)));
    }

    /// The watch scope has to match the RBAC the chart rendered, or the
    /// operator watches nothing: `Api::all` is a cluster-wide LIST and no
    /// `Role` can authorise one. An empty or absent list is the cluster-wide
    /// default, and a trailing comma must not become a watch on `""` — the API
    /// server reads that as every namespace, which is the 403 this exists to
    /// avoid.
    #[test]
    fn a_namespace_list_is_the_chart_s_and_an_empty_one_is_the_whole_cluster() {
        assert_eq!(namespaces(None), None);
        assert_eq!(namespaces(Some("")), None);
        assert_eq!(namespaces(Some(" , ")), None);
        assert_eq!(namespaces(Some("alpha")), Some(vec!["alpha".into()]));
        assert_eq!(
            namespaces(Some("alpha, beta,")),
            Some(vec!["alpha".into(), "beta".into()])
        );
    }

    /// The cooldown is read live, for the same reason `draining` is.
    ///
    /// `scale()` fires the `.owns(StatefulSet)` watch before the status write
    /// stamping `lastScaled` has even been issued, so the reconcile that event
    /// triggers is handed a cached object from *before* the stamp. Deciding off
    /// that is deciding with no cooldown at all: the tier acts again on a
    /// reading that predates its own last scale, before the new volume has been
    /// bound.
    #[tokio::test]
    async fn a_cooldown_the_cached_copy_has_not_caught_up_with_still_holds() {
        let f = Fake::live(MiraClusterStatus {
            last_scaled: Some(fmt_rfc3339(now_secs())),
            ..Default::default()
        });
        let c = drainable();
        assert!(c.status.as_ref().unwrap().last_scaled.is_none());

        let action = reconcile(Arc::new(c), f.ctx()).await.unwrap();

        assert!(
            !f.log().iter().any(|l| l.contains("/status")),
            "a pass inside the cooldown decides nothing and writes nothing: {:?}",
            f.log()
        );
        let held = [599, 600].map(|s| Action::requeue(Duration::from_secs(s)));
        assert!(
            held.contains(&action),
            "the requeue is the cooldown's remainder, not the 60s decision interval: {action:?}"
        );
    }

    /// And the branch that runs once it has expired has to write it back.
    ///
    /// Server-side apply removes a field this manager owned and then omitted,
    /// so the ordinary `Ready` write at the end of a quiet pass was deleting
    /// the stamp the scale before it had left. One quiet pass to forget, and
    /// the next reading was free to swing the tier again.
    #[tokio::test]
    async fn a_quiet_pass_does_not_delete_the_cooldown_it_has_outlived() {
        let stamped = fmt_rfc3339(now_secs() - 5_000);
        let f = Fake::live(MiraClusterStatus {
            last_scaled: Some(stamped.clone()),
            ..Default::default()
        });

        reconcile(Arc::new(drainable()), f.ctx()).await.unwrap();

        let st = f.body("/status")["status"].clone();
        assert_eq!(st["phase"], "Ready");
        assert_eq!(st["lastScaled"], stamped, "{st}");
    }
}
