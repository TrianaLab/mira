//! The reconcile loop.
//!
//! # The scale-in sequence, and why it is in this order
//!
//! Shrinking a Mira tier is not `replicas -= 1`. The volume about to go holds
//! blocks no other replica has — nothing replicates and nothing rebalances
//! (architecture.md section 12.4) — so they have to be copied out first, and
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
//!
//! Every step before 4 is reversible, and step 4 is gated on the archive
//! existing. A failed drain leaves the claim in place and the phase `Degraded`:
//! the tier is one replica smaller and the blocks are still on disk, which is a
//! bad day rather than a data-loss incident.
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
use kube::api::{Api, Patch, PatchParams, PostParams};
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

    if let Err(e) = c.spec.validate() {
        // A spec that cannot be satisfied is reported and then left alone. Not
        // requeued fast: nothing the controller does will fix it, and a hot
        // loop on an invalid object is how an operator takes out an API server.
        status(&ctx, &c, json!({"phase": "Degraded", "message": e})).await?;
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
    ensure(&ctx, &c, &ns, current).await?;

    // A drain already in flight owns the next transition. Re-reading stats and
    // deciding again here would be deciding from a tier that is mid-shrink.
    if let Some(ordinal) = c.status.as_ref().and_then(|s| s.draining) {
        return finish_drain(&ctx, &c, &ns, ordinal).await;
    }

    if let Some(remaining) = cooling_down(&c) {
        return Ok(Action::requeue(remaining));
    }

    let readings = read_all(&c, current).await;
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
                &c,
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
                &c,
                json!({
                    "replicas": current,
                    "phase": "Degraded",
                    "freeFraction": free,
                    "message": format!(
                        "free is below {} but the tier is at maxReplicas ({})",
                        c.spec.scaling.up_when_free_below, c.spec.max_replicas
                    ),
                }),
            )
            .await?;
        }
        Decision::Down if current > c.spec.replicas => match c.spec.offload.as_deref() {
            Some(_) => return start_drain(&ctx, &c, &ns, current, free).await,
            // The deliberate refusal. Scaling in without an archive deletes the
            // only copy of those blocks, and the cost of *not* shrinking is a
            // bill rather than the data.
            None => {
                status(
                    &ctx,
                    &c,
                    json!({
                        "replicas": current,
                        "phase": "Ready",
                        "freeFraction": free,
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
                &c,
                json!({"replicas": current, "phase": "Ready", "freeFraction": free, "message": null}),
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
    let deps: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);

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

    if c.spec.proxy.replicas > 0 {
        let pn = res::proxy_name(c);
        cms.patch(&pn, &params(), &apply(&res::proxy_config_map(c, replicas)))
            .await?;
        svcs.patch(&pn, &params(), &apply(&res::proxy_service(c)))
            .await?;
        deps.patch(&pn, &params(), &apply(&res::proxy_deployment(c, replicas)))
            .await?;
    }
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
            "message": format!("draining replica {ordinal} before removing its volume"),
        }),
    )
    .await?;

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
        if let Err(e) = pvcs.delete(&claim, &Default::default()).await {
            warn!(%ns, %claim, "archive complete but claim not deleted: {e}");
        }
        info!(%ns, ordinal, "drain complete, volume released");
        status(
            ctx,
            c,
            json!({
                "phase": "Ready",
                "draining": null,
                "lastScaled": now(),
                "message": format!("replica {ordinal} archived and removed"),
            }),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    if st.failed.unwrap_or(0) > 0 {
        // Stop here, loudly, with the claim intact. The tier is one replica
        // smaller and the blocks are still on disk.
        error!(%ns, ordinal, "drain failed; volume kept");
        status(
            ctx,
            c,
            json!({
                "phase": "Degraded",
                "message": format!(
                    "drain of replica {ordinal} failed; its volume was kept. \
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

/// Run the controller until the process is asked to stop.
pub async fn run(client: Client) -> Result<(), kube::Error> {
    let clusters: Api<MiraCluster> = Api::all(client.clone());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });

    Controller::new(clusters, Config::default())
        // Owned objects, so a StatefulSet somebody edited by hand is corrected
        // on the spot rather than at the next poll.
        .owns(Api::<StatefulSet>::all(client.clone()), Config::default())
        .owns(Api::<Deployment>::all(client.clone()), Config::default())
        .owns(Api::<Job>::all(client), Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                error!("controller: {e}");
            }
        })
        .await;
    Ok(())
}

// Time, without a date library. The only two operations this file needs are
// "now, as RFC3339" and "how long since that string", and both are a handful of
// integer arithmetic on a Unix timestamp — cheaper than a dependency whose
// timezone database is irrelevant to a cooldown measured in minutes.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds as RFC3339 UTC.
fn now() -> String {
    fmt_rfc3339(now_secs())
}

fn fmt_rfc3339(secs: i64) -> String {
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
fn chrono_parse(s: &str) -> Option<i64> {
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
    use crate::crd::{MiraClusterSpec, MiraClusterStatus, Proxy, Scaling, Storage};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

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
                scaling: Scaling {
                    cooldown_seconds: cooldown,
                    ..Default::default()
                },
                offload: None,
                proxy: Proxy::default(),
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
}
