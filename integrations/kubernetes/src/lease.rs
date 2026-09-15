//! One writer for the tier, enforced by a `coordination.k8s.io` Lease.
//!
//! # What goes wrong without it
//!
//! Every scale decision is read-then-write: read `free_fraction` from each
//! replica, compare against the thresholds, write `spec.replicas`. Two
//! controllers run that against the same tier from the same readings and both
//! decide to add a replica — the tier moves two, for one decision, and the
//! cooldown that exists to stop exactly that is written by whichever one
//! patched the status last. A scale-*in* doubles it: both pick the top
//! ordinal, both write `status.draining`, and the second one's `finish_drain`
//! deletes the claim the first one is still archiving.
//!
//! The chart's `strategy: Recreate` already prevents the overlap a rolling
//! update would create, and that was the whole mitigation. It does not cover
//! `replicaCount: 2`, which is a values.yaml knob and reads like a sensible
//! thing to set for availability.
//!
//! # Soft, and what that word is doing
//!
//! Two limits, stated rather than papered over:
//!
//! * **It is not a fence.** A lease is held for `DURATION`, and a holder whose
//!   clock is slow, or whose renewal is stuck behind a blocked API call, can
//!   still be inside a reconcile when a contender's clock says the lease
//!   expired. Fencing that would need a token the API server checks on every
//!   write, and there is none. What this buys is that the window is seconds of
//!   overlap after a failure rather than permanent concurrency by
//!   configuration.
//! * **It is per-install, and the watch is not.** The Lease lives in the
//!   operator's own namespace; [`controller::run`] watches `Api::all`. So two
//!   *separate* installs in two namespaces hold two different leases and both
//!   reconcile every MiraCluster in the cluster. That is a cluster-wide
//!   operator installed twice, and this module does not detect it.
//!
//! [`controller::run`]: crate::controller::run

use std::time::Duration;

use k8s_openapi::api::coordination::v1::Lease as CoordLease;
use kube::api::{Api, PostParams};
use kube::{Client, Resource, ResourceExt};
use serde_json::json;
use tracing::{info, warn};

use crate::controller::{chrono_parse, fmt_rfc3339, now_secs};

/// The lease object's name, in the operator's own namespace.
const NAME: &str = "mira-operator";

/// How long a holder's claim survives without a renewal.
///
/// Thirty seconds against a ten-second renewal, so two renewals have to be
/// missed before a standby may act. The cost of being slow here is a scaling
/// pause; the cost of being fast is two controllers, which is the failure the
/// whole module exists for. The pause loses nothing — Mira keeps ingesting and
/// serving with no controller at all.
const DURATION: i64 = 30;

/// How often the holder writes `renewTime`.
const RENEW: Duration = Duration::from_secs(10);

/// How often a standby re-checks.
const POLL: Duration = Duration::from_secs(5);

pub struct Lease {
    api: Api<CoordLease>,
    id: String,
}

impl Lease {
    /// `ns` is the operator's own namespace — from the service account
    /// in-cluster, from the kubeconfig context on a laptop.
    pub fn new(client: &Client, ns: &str, id: String) -> Self {
        Self {
            api: Api::namespaced(client.clone(), ns),
            id,
        }
    }

    /// Take the lease if it is free or stale. `false` if somebody else holds a
    /// live one.
    ///
    /// The write is `replace` and not a patch, because `replace` carries the
    /// `resourceVersion` this read returned: two contenders that both saw the
    /// same expired lease both try to claim it, and the API server accepts one
    /// and answers the other with a 409. Server-side apply would accept both.
    pub async fn try_hold(&self) -> Result<bool, kube::Error> {
        let now = now_secs();
        let Some(held) = self.api.get_opt(NAME).await? else {
            // Nobody has ever held it. `create` is the same race as above with
            // the same resolution: the second one gets 409 AlreadyExists.
            let fresh = self.claim(None, now, 0);
            return match self.api.create(&PostParams::default(), &fresh).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                Err(e) => Err(e),
            };
        };

        let doc = serde_json::to_value(&held).unwrap_or_default();
        let spec = &doc["spec"];
        let holder = spec["holderIdentity"].as_str();
        let ours = holder == Some(&*self.id);
        let renewed = spec["renewTime"].as_str().and_then(chrono_parse);

        // Not ours, and still being renewed. Note the `None` arm falls through
        // to the claim: a Lease object with no `renewTime` is one nothing has
        // ever renewed, and waiting DURATION for a timestamp that will never
        // appear would wedge the operator on an empty object.
        if !ours && renewed.is_some_and(|t| now - t < DURATION) {
            return Ok(false);
        }

        // `leaseTransitions` counts handovers and not renewals, which is what
        // makes it worth reading: a number that climbs while the tier is
        // healthy is two operators trading the lease back and forth, and that
        // is invisible in a log where each of them only ever says "acquired".
        let transitions = spec["leaseTransitions"].as_i64().unwrap_or(0);
        let (acquired, transitions) = match ours {
            // Ours already: keep the original `acquireTime`, so `kubectl get
            // lease` reports how long this holder has actually been leading.
            true => (spec["acquireTime"].as_str().map(str::to_owned), transitions),
            false => (None, transitions + 1),
        };

        let mut next = self.claim(acquired, now, transitions);
        // The version this read returned. Dropping it turns the optimistic
        // update into an unconditional one, which is the whole guarantee.
        next.meta_mut().resource_version = held.resource_version();

        match self.api.replace(NAME, &PostParams::default(), &next).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Block until the lease is held. A standby sits here; it is not an error
    /// state and it is not a crash loop.
    pub async fn hold(&self) -> Result<(), kube::Error> {
        let mut waiting = false;
        loop {
            if self.try_hold().await? {
                info!(id = %self.id, "holding the operator lease");
                return Ok(());
            }
            if !std::mem::replace(&mut waiting, true) {
                info!(id = %self.id, "another operator holds the lease; standing by");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Renew for as long as this process is the holder, and stop the process
    /// when it is not.
    ///
    /// Exiting rather than unwinding, because there is no safe way back: the
    /// controller is mid-flight against a tier another operator now owns, and
    /// finishing whatever reconcile is in progress is the concurrency this
    /// module exists to prevent. The Deployment restarts the pod and it comes
    /// back up in [`hold`], waiting its turn. A transient API error is not
    /// that — the lease is still ours until it expires, so those are warned
    /// about and retried.
    ///
    /// [`hold`]: Lease::hold
    pub async fn renew_forever(self) -> ! {
        loop {
            tokio::time::sleep(RENEW).await;
            match self.try_hold().await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(id = %self.id, "lost the operator lease; stopping");
                    std::process::exit(1);
                }
                Err(e) => warn!(id = %self.id, error = %e, "cannot renew the operator lease"),
            }
        }
    }

    fn claim(&self, acquired: Option<String>, now: i64, transitions: i64) -> CoordLease {
        let stamp = fmt_rfc3339(now);
        serde_json::from_value(json!({
            "metadata": {"name": NAME},
            "spec": {
                "holderIdentity": self.id,
                "leaseDurationSeconds": DURATION,
                "acquireTime": acquired.unwrap_or_else(|| stamp.clone()),
                "renewTime": stamp,
                "leaseTransitions": transitions,
            },
        }))
        .expect("lease is well-formed")
    }
}

/// Who this process says it is, in the lease and in `kubectl get lease`.
///
/// `POD_NAME` comes from the downward API in the chart's Deployment. The
/// fallback is for `cargo run` against a kind cluster, where there is no pod
/// and the only thing that has to be true is that a second `cargo run` on the
/// same laptop is a different identity.
pub fn identity() -> String {
    std::env::var("POD_NAME").unwrap_or_else(|_| format!("local-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Fake, conflict, not_found};
    use http::StatusCode;

    fn lease(holder: &str, age: i64, transitions: i64) -> serde_json::Value {
        json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {"name": NAME, "namespace": "mira-system", "resourceVersion": "42"},
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": DURATION,
                "acquireTime": fmt_rfc3339(now_secs() - 3_600),
                "renewTime": fmt_rfc3339(now_secs() - age),
                "leaseTransitions": transitions,
            },
        })
    }

    fn get(doc: serde_json::Value) -> Fake {
        Fake::new(move |method, _| match method {
            "GET" => (StatusCode::OK, doc.clone()),
            _ => (StatusCode::OK, lease("us", 0, 0)),
        })
    }

    /// Unix seconds out of a serialized `MicroTime`, which round-trips
    /// `…:19Z` back as `…:19.000000Z` — so the two have to be compared as
    /// instants and not as strings.
    fn stamp(v: &serde_json::Value) -> i64 {
        chrono_parse(v.as_str().expect("a timestamp")).expect("parses")
    }

    fn subject(f: &Fake) -> Lease {
        Lease::new(&f.client("mira-system"), "mira-system", "us".into())
    }

    /// The case the module exists for: somebody else is leading and saying so.
    #[tokio::test]
    async fn a_lease_another_operator_is_renewing_is_not_taken() {
        let f = get(lease("them", 2, 0));
        assert!(!subject(&f).try_hold().await.unwrap());
        assert_eq!(
            f.log(),
            ["GET /apis/coordination.k8s.io/v1/namespaces/mira-system/leases/mira-operator"]
        );
    }

    /// And the case that makes it a lease rather than a lock: the holder is
    /// gone. Nothing deletes the object when a pod dies, so the only evidence
    /// is a `renewTime` that stopped moving.
    #[tokio::test]
    async fn a_holder_that_stopped_renewing_is_taken_over_and_counted() {
        let f = get(lease("them", DURATION + 5, 3));
        assert!(subject(&f).try_hold().await.unwrap());

        let put = f.body("PUT");
        assert_eq!(put["spec"]["holderIdentity"], "us");
        // A handover, so the counter moves and `acquireTime` restarts. Both are
        // what `kubectl get lease` shows an operator debugging a flapping tier.
        assert_eq!(put["spec"]["leaseTransitions"], 4);
        assert_eq!(stamp(&put["spec"]["acquireTime"]), now_secs());
        // The version the read returned, or the update is unconditional and
        // two contenders both win.
        assert_eq!(put["metadata"]["resourceVersion"], "42");
    }

    /// Renewing our own is not a handover. Resetting either field here would
    /// make a healthy holder look like it had just won a contested election,
    /// every ten seconds.
    #[tokio::test]
    async fn renewing_our_own_lease_is_not_a_transition() {
        let held = lease("us", 5, 3);
        let f = get(held.clone());
        assert!(subject(&f).try_hold().await.unwrap());

        let put = f.body("PUT");
        assert_eq!(put["spec"]["leaseTransitions"], 3);
        assert_eq!(stamp(&put["spec"]["acquireTime"]), now_secs() - 3_600);
        assert_eq!(stamp(&put["spec"]["renewTime"]), now_secs());
    }

    #[tokio::test]
    async fn a_first_start_creates_the_lease() {
        let f = Fake::new(|method, _| match method {
            "GET" => (StatusCode::NOT_FOUND, not_found()),
            _ => (StatusCode::OK, lease("us", 0, 0)),
        });
        assert!(subject(&f).try_hold().await.unwrap());
        assert_eq!(f.body("POST")["spec"]["holderIdentity"], "us");
    }

    /// Both halves of the race, and neither is an error. A 409 means the API
    /// server picked the other contender, which is the mechanism working —
    /// propagated as an error it would land in the caller's retry path and be
    /// logged as a failure once every five seconds on every standby.
    #[tokio::test]
    async fn losing_the_race_is_a_no_rather_than_a_failure() {
        let created = Fake::new(|method, _| match method {
            "GET" => (StatusCode::NOT_FOUND, not_found()),
            _ => (StatusCode::CONFLICT, conflict()),
        });
        assert!(!subject(&created).try_hold().await.unwrap());

        let expired = Fake::new(|method, _| match method {
            "GET" => (StatusCode::OK, lease("them", DURATION + 5, 0)),
            _ => (StatusCode::CONFLICT, conflict()),
        });
        assert!(!subject(&expired).try_hold().await.unwrap());
    }

    /// A standby is not a failure and not a crash loop: it polls, and the
    /// moment the incumbent stops renewing it leads. `start_paused` is what
    /// makes that assertable — tokio advances its own clock when every task is
    /// idle, so the `POLL` sleeps cost no wall time.
    #[tokio::test(start_paused = true)]
    async fn a_standby_waits_until_the_incumbent_stops_renewing() {
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let f = Fake::new(move |method, _| match method {
            "GET" => {
                let n = polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let age = if n < 3 { 2 } else { DURATION + 5 };
                (StatusCode::OK, lease("them", age, 0))
            }
            _ => (StatusCode::OK, lease("us", 0, 0)),
        });
        subject(&f).hold().await.unwrap();
        // Three refusals and then the claim, rather than one attempt that
        // returned an error to a caller with nowhere to put it.
        assert_eq!(f.log().iter().filter(|l| l.starts_with("GET")).count(), 4);
        assert_eq!(f.body("PUT")["spec"]["holderIdentity"], "us");
    }

    /// A Lease object somebody created by hand, or one left by an operator that
    /// died before its first renewal. Read as "held until it expires" this
    /// would wait `DURATION` for a timestamp that is never going to appear, and
    /// on a `leaseDurationSeconds` somebody had typed as 86400, for ever.
    #[tokio::test]
    async fn a_lease_nothing_has_ever_renewed_is_free() {
        let f = get(json!({
            "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": {"name": NAME, "namespace": "mira-system", "resourceVersion": "1"},
            "spec": {},
        }));
        assert!(subject(&f).try_hold().await.unwrap());
        assert_eq!(f.body("PUT")["spec"]["holderIdentity"], "us");
    }
}
