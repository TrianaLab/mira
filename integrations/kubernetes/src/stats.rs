//! Reading `/api/v1/stats` off the replicas, and turning it into a decision.
//!
//! Plain HTTP, over hyper, with no TLS client anywhere in this module. That is
//! not an oversight: the engine's own proxy refuses any scheme but `http://`
//! for the replica list, on the grounds that a replica is "this deployment's
//! own node on its own network". The operator talks to the same addresses over
//! the same network and has no reason to be stricter than the thing whose job
//! it is. hyper is already in the tree underneath the Kubernetes client, so
//! this costs no crate.

use std::time::Duration;

use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// What one replica said when asked.
///
/// Three states and not two. `free_fraction` is `null` in the engine's own
/// response when `statfs` would not answer, and its comment is explicit that
/// "no blocks" and "I could not look" are different operational facts that a
/// zero would conflate. Collapsing that `null` into a number here would undo
/// the distinction at the one place it decides something: read as 0.0 it means
/// "full, scale out", and read as 1.0 it means "empty, scale in". Both are
/// inventions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    /// The replica answered with a number.
    Free(f64),
    /// The replica answered, but could not stat its own filesystem.
    Unknown,
    /// The replica did not answer.
    Unreachable,
}

/// Ask one replica for its stats.
///
/// A timeout rather than hyper's default of none. An unreachable replica that
/// never resets the connection would otherwise hold the whole reconcile open,
/// and the reconcile holds the decision for every *other* replica with it.
pub async fn read(base: &str, timeout: Duration) -> Reading {
    let uri = format!("{base}/api/v1/stats");
    let client: Client<_, String> = Client::builder(TokioExecutor::new())
        .build(hyper_util::client::legacy::connect::HttpConnector::new());

    let fetch = async {
        let res = client.get(uri.parse().ok()?).await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let body = res.into_body().collect().await.ok()?.to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
        Some(v)
    };

    match tokio::time::timeout(timeout, fetch).await {
        Ok(Some(v)) => match v.get("free_fraction") {
            // `as_f64` on a JSON null is None, which is exactly the case the
            // engine emits null for. Matching the key's *presence* separately
            // from its parse keeps a missing key (an older Mira, a different
            // endpoint) from silently reading as the same thing.
            Some(serde_json::Value::Number(n)) => {
                n.as_f64().map(Reading::Free).unwrap_or(Reading::Unknown)
            }
            _ => Reading::Unknown,
        },
        Ok(None) | Err(_) => Reading::Unreachable,
    }
}

/// What the operator should do about the tier as a whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// Add one replica.
    Up,
    /// Drain and remove the highest-ordinal replica.
    Down,
    /// Change nothing.
    Hold,
}

/// Fold the replicas' readings into one action.
///
/// The asymmetry between the two directions is the whole point of this
/// function, and it is not a style choice:
///
/// - **Scale out on the worst reading.** Blocks are not evenly spread, because
///   `route` sends a resource to `hash(resource) % n` and resources are not the
///   same size. A mean of 0.4 across ten replicas is compatible with one at
///   0.02, and it is the one at 0.02 that stops accepting writes. So one
///   replica below the floor is enough.
///
/// - **Scale in only on unanimity.** Removing a replica moves its whole dataset
///   through `offload push` and then deletes a volume. Doing that because the
///   average looked roomy, while one replica is nearly full, adds that
///   replica's share of the redistributed load to the fullest node in the tier.
///
/// - **Any doubt holds.** An unreachable or unknown replica is not evidence of
///   room. Scaling out on it would be acting on a pod that is merely starting;
///   scaling in on it would be deleting a volume whose occupancy nobody can
///   currently see. `Unknown` is the engine saying it could not stat its own
///   filesystem — the one reading that must never be rounded to a number.
pub fn decide(readings: &[Reading], up_below: f64, down_above: f64) -> Decision {
    if readings.is_empty() {
        return Decision::Hold;
    }
    // One silent replica and the tier holds. The alternative is deciding from a
    // partial view of a quantity whose whole purpose is to be the minimum.
    if readings.iter().any(|r| !matches!(r, Reading::Free(_))) {
        return Decision::Hold;
    }
    let free: Vec<f64> = readings
        .iter()
        .filter_map(|r| match r {
            Reading::Free(f) => Some(*f),
            _ => None,
        })
        .collect();

    if free.iter().any(|&f| f < up_below) {
        return Decision::Up;
    }
    if free.iter().all(|&f| f > down_above) {
        return Decision::Down;
    }
    Decision::Hold
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: f64 = 0.15;
    const DOWN: f64 = 0.60;

    /// The distribution argument, as a test. Nine roomy replicas and one nearly
    /// full is a mean of ~0.7 and a tier that is about to start refusing writes
    /// on one node. A mean-based rule scales *in* here.
    #[test]
    fn one_full_replica_outvotes_nine_empty_ones() {
        let mut r = vec![Reading::Free(0.8); 9];
        r.push(Reading::Free(0.02));
        assert_eq!(decide(&r, UP, DOWN), Decision::Up);
    }

    /// The mirror: scale-in needs every replica roomy, because the drained
    /// replica's share lands on whoever is left.
    #[test]
    fn scale_in_needs_every_replica_roomy_not_the_average() {
        assert_eq!(
            decide(&[Reading::Free(0.9), Reading::Free(0.9)], UP, DOWN),
            Decision::Down
        );
        // One replica in the band is enough to hold, even though the mean is
        // comfortably above the threshold.
        assert_eq!(
            decide(&[Reading::Free(0.95), Reading::Free(0.5)], UP, DOWN),
            Decision::Hold
        );
    }

    /// A `null` `free_fraction` is the engine saying it could not stat the
    /// filesystem. Read as 0.0 it scales out forever; read as 1.0 it deletes a
    /// volume. It must do neither.
    #[test]
    fn an_unknown_or_unreachable_replica_stops_every_decision() {
        for bad in [Reading::Unknown, Reading::Unreachable] {
            // Would be Down on the numbers alone.
            assert_eq!(decide(&[Reading::Free(0.9), bad], UP, DOWN), Decision::Hold);
            // Would be Up on the numbers alone.
            assert_eq!(
                decide(&[Reading::Free(0.01), bad], UP, DOWN),
                Decision::Hold
            );
        }
    }

    /// The hysteresis band itself: a reading between the thresholds is the
    /// steady state and must produce no action in either direction.
    #[test]
    fn a_reading_inside_the_band_holds() {
        for f in [0.16, 0.3, 0.5, 0.59] {
            assert_eq!(decide(&[Reading::Free(f)], UP, DOWN), Decision::Hold, "{f}");
        }
        // And the boundaries are exclusive on both sides, so a reading sitting
        // exactly on a threshold does not flap between two reconciles.
        assert_eq!(decide(&[Reading::Free(UP)], UP, DOWN), Decision::Hold);
        assert_eq!(decide(&[Reading::Free(DOWN)], UP, DOWN), Decision::Hold);
    }

    /// No replicas is not "empty, scale in" — it is a tier that has not started.
    #[test]
    fn no_readings_holds() {
        assert_eq!(decide(&[], UP, DOWN), Decision::Hold);
    }
}
