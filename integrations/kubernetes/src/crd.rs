//! The `MiraCluster` custom resource.
//!
//! # Why a CRD and not an HPA
//!
//! A HorizontalPodAutoscaler scales on a metric, and Mira has no metric that
//! moves when it needs more replicas. Section 11 measures 2.23 of twelve cores
//! at 1,537,875 records/s — a CPU-target HPA reads single-digit utilisation at
//! saturation, and a memory-target one reads page cache, which is the mmap
//! working as designed. The quantity that actually runs out is *disk*, and the
//! HPA has never had a way to scale a StatefulSet on its own volumes filling.
//!
//! So the trigger is `free_fraction` off `/api/v1/stats`, and something has to
//! read it. That something also has to sequence a scale-in, because shrinking a
//! Mira tier is not `replicas -= 1`: the volume about to be deleted holds
//! blocks no other replica has, and they have to be copied out first.
//!
//! # Why this does not violate principle 4
//!
//! Principle 4 says *Mira* holds no coordination state — no Raft, no
//! membership, no external metadata store, the block directory is the manifest.
//! A controller is not Mira. It is the same delegation the engine already makes
//! to the platform at `block.rs:402-412` and argues for in architecture.md
//! section 12.3, moved up one level: Kubernetes already knows the membership,
//! and reading it from the API server is not a consensus protocol.
//!
//! The test of that claim is what happens when the operator is deleted. Every
//! Mira pod keeps serving, keeps ingesting and keeps its blocks readable,
//! because none of them ever asked the operator anything. Only the *scaling*
//! stops. That is the line between a coordinator and coordination state.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A Mira storage tier and the proxy in front of it.
///
/// The operator owns the whole topology rather than autoscaling a StatefulSet
/// somebody else installed, and that is deliberate. A controller that only
/// writes `spec.replicas` on an object Helm owns loses the value back on the
/// next `helm upgrade`, which reasserts the replica count from the chart — the
/// scale-out silently unwinds, and on the way down it unwinds *after* the drain
/// has already copied the blocks out. Owning the StatefulSet means one writer
/// for the field that matters.
///
/// This is the only way in. The chart that installed a StatefulSet directly was
/// removed rather than kept beside it: two charts is two answers to "how do I
/// run Mira on Kubernetes", and the one that cannot scale, cannot drain and
/// cannot be told a ceiling is the wrong default.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "mira.miradb.dev",
    version = "v1alpha1",
    kind = "MiraCluster",
    shortname = "mira",
    namespaced,
    status = "MiraClusterStatus",
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".status.replicas"}"#,
    printcolumn = r#"{"name":"Free","type":"string","jsonPath":".status.freeFraction"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    // Enforced by the apiserver, not only by `validate()`. Both exist and they
    // are not redundant: a rule here rejects the `kubectl apply` itself, where
    // the reconciler can only accept the object and write `Degraded` on its
    // next pass — so whoever applied a ceiling below the floor learns it from
    // the command they typed rather than from a status field they have to know
    // to read. `validate()` stays because the apiserver is not guaranteed to
    // have evaluated this: a CR created before the rule was added is already
    // stored, and CEL cost limits can make a rule non-enforcing at admission.
    //
    // `self.spec` and not `self`: kube-derive attaches a struct-level rule to
    // the *root* schema, where `self` is the whole custom resource. `spec` is
    // in the root's `required`, and both fields have schema defaults that the
    // apiserver applies before it evaluates this, so the two lookups cannot be
    // missing.
    validation = Rule::new("self.spec.maxReplicas >= self.spec.replicas")
        .message("maxReplicas must be at least replicas")
)]
#[serde(rename_all = "camelCase")]
pub struct MiraClusterSpec {
    /// The Mira image the storage pods and the drain Jobs both run.
    ///
    /// One field and not two: the drain is `mira offload push`, the same binary
    /// and the same subcommand the tier already ships. An operator that could
    /// run a *different* Mira against the volume than the one that wrote it is
    /// a version-skew bug waiting for a scale-in.
    pub image: String,

    /// Replica floor. The tier never shrinks below this.
    #[serde(default = "default_replicas")]
    #[schemars(range(min = 1))]
    pub replicas: i32,

    /// Replica ceiling. Scale-out stops here rather than filling the cluster.
    ///
    /// There is no "unbounded" value on purpose. The scale trigger is a disk
    /// that is filling, and a runaway ingest with no ceiling turns one full
    /// volume into every volume.
    #[serde(default = "default_max_replicas")]
    #[schemars(range(min = 1))]
    pub max_replicas: i32,

    /// Per-replica volume.
    pub storage: Storage,

    /// When to add a replica, and when to take one away.
    #[serde(default)]
    pub scaling: Scaling,

    /// Where a drained replica's blocks are copied before its volume is
    /// deleted. `${node}` interpolates to the replica name.
    ///
    /// Required for scale-in and only for scale-in. With it unset the operator
    /// still scales *out*, and a `Decision::Down` lands on `.status` as a
    /// message instead of removing a pod — see `controller::reconcile`. That is
    /// the safe direction to fail in: the cost of not shrinking is a bill, and
    /// the cost of shrinking without an archive is the data.
    ///
    /// It reaches the *drain Job* and nothing else. A running replica never
    /// sees it: `storage.offload` on a node is a retention setting that unlinks
    /// each block it copies, and the replicas mount no cold volume — see
    /// `resources::node_config`.
    ///
    /// `file://` is the only scheme Mira's offload target parses, so on
    /// Kubernetes this path has to be a mount — hence `coldStorageClaim`, which
    /// is required alongside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload: Option<String>,

    /// Name of an existing claim holding the cold store, mounted into the drain
    /// Job at the first path segment of `offload`.
    ///
    /// This field exists because its absence was silent data loss. `offload` is
    /// a `file://` URL — the only scheme there is — and a drain Job that mounts
    /// nothing writes the archive into its own container filesystem, exits 0,
    /// and the operator then deletes the claim it just "archived". The blocks
    /// are gone and every status field says `Ready`.
    ///
    /// So the pair is validated rather than documented: `offload` without this
    /// is refused, which makes the losing configuration unrepresentable instead
    /// of merely discouraged.
    ///
    /// `ReadWriteMany` if the tier can drain on more than one node. A drain runs
    /// one at a time, so `ReadWriteOnce` is enough on a single-node cluster and
    /// will strand the Job anywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_storage_claim: Option<String>,

    /// The `mira proxy` tier that fans out across the replicas.
    #[serde(default)]
    pub proxy: Proxy,
}

/// Per-replica persistent volume.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Storage {
    /// Volume size, as a Kubernetes quantity — `100Gi`.
    pub size: Quantity,

    /// StorageClass. Unset uses the cluster default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
}

/// The two thresholds and the ceiling on how fast either fires.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Scaling {
    /// Add a replica when the *fullest* replica's `free_fraction` drops below
    /// this.
    ///
    /// The fullest and not the mean: blocks are not evenly distributed, because
    /// `route` sends a resource's spans to `hash(resource) % n` and resources
    /// are not the same size. A mean of 0.4 across ten replicas is compatible
    /// with one of them at 0.02, and it is the one at 0.02 that stops accepting
    /// writes.
    #[serde(default = "default_scale_up_below")]
    pub up_when_free_below: f64,

    /// Remove a replica when *every* replica's `free_fraction` is above this.
    ///
    /// Every and not the mean, for the mirror of the reason above, and the gap
    /// between the two thresholds is the hysteresis band. They must not meet:
    /// adjacent thresholds put the tier in a scale-out/scale-in loop where each
    /// action creates the condition for the other, and every cycle of that loop
    /// moves the whole dataset of one replica through `offload push`.
    #[serde(default = "default_scale_down_above")]
    pub down_when_free_above: f64,

    /// Seconds after any scaling action before another may be considered.
    ///
    /// A scale-out needs a new volume bound, a pod scheduled and a proxy roll
    /// before its effect on `free_fraction` is visible. Acting again inside
    /// that window is acting on a reading that predates the last decision.
    #[serde(default = "default_cooldown")]
    pub cooldown_seconds: i64,

    /// Seconds a drain Job may run before Kubernetes fails it.
    ///
    /// `backoffLimit` bounds how many times the drain is *retried*, not how
    /// long one attempt runs. A copy blocked on an unresponsive cold store
    /// stays `active` indefinitely, and the reconcile that is waiting on it has
    /// nothing to wait *for* — the tier sits at `Draining` with one replica
    /// already gone and no signal that anything is wrong.
    ///
    /// An hour by default, which is a copy rate rather than a guess: it is a
    /// 100 GiB volume at 30 MB/s, the low end of a network-backed cold store.
    /// Raise it for a volume that cannot finish in that, because the cost of a
    /// deadline that is too short is a `Degraded` tier with its blocks intact,
    /// and the cost of one that is too long is only that the operator notices
    /// later.
    #[serde(default = "default_drain_deadline")]
    pub drain_deadline_seconds: i64,
}

/// The stateless read tier.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Proxy {
    /// How many proxy pods. Zero disables the proxy entirely and leaves the
    /// storage pods addressable only through the headless Service.
    #[serde(default = "default_proxy_replicas")]
    pub replicas: i32,
}

/// What the operator observed and what it did about it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MiraClusterStatus {
    /// Replica count the operator is currently asking for.
    #[serde(default)]
    pub replicas: i32,

    /// The lowest `free_fraction` seen across the tier, rendered to two
    /// decimals. A string and not a float because this is a `printcolumn`, and
    /// `kubectl` renders a float64 status field in scientific notation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_fraction: Option<String>,

    /// `Ready`, `ScalingUp`, `Draining` or `Degraded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,

    /// RFC3339 of the last completed scaling action, for the cooldown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scaled: Option<String>,

    /// Replica ordinal currently being drained, if any. Set before the drain
    /// Job is created and cleared only once the volume has been released, so a
    /// controller restart mid-drain resumes rather than abandoning a half-copied
    /// tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draining: Option<i32>,

    /// Human-readable reason for the current phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn default_replicas() -> i32 {
    1
}
fn default_max_replicas() -> i32 {
    10
}
fn default_proxy_replicas() -> i32 {
    2
}
fn default_cooldown() -> i64 {
    600
}
fn default_drain_deadline() -> i64 {
    3600
}

// 0.15/0.60 rather than something tighter. `free_fraction` is whole-filesystem,
// so it moves when anything else on the volume moves, and the band has to be
// wide enough that noise cannot cross it. The floor is also not arbitrary:
// `pipeline` starts refusing writes when the disk fills, and a scale-out needs
// a volume bound and a pod scheduled before it relieves anything — 15% is the
// headroom that buys.
fn default_scale_up_below() -> f64 {
    0.15
}
fn default_scale_down_above() -> f64 {
    0.60
}

impl Default for Scaling {
    fn default() -> Self {
        Self {
            up_when_free_below: default_scale_up_below(),
            down_when_free_above: default_scale_down_above(),
            cooldown_seconds: default_cooldown(),
            drain_deadline_seconds: default_drain_deadline(),
        }
    }
}

impl Default for Proxy {
    fn default() -> Self {
        Self {
            replicas: default_proxy_replicas(),
        }
    }
}

impl MiraClusterSpec {
    /// Reject a spec whose thresholds would oscillate, before it is acted on.
    ///
    /// The CRD's own schema cannot express this: it is a relation between two
    /// fields, and structural schemas only bound them one at a time. Checking
    /// it in the reconciler and surfacing it as `Degraded` is the difference
    /// between a tier that refuses to scale and says why, and one that scales
    /// both directions forever while `offload push` copies a replica's whole
    /// dataset on every cycle.
    pub fn validate(&self) -> Result<(), String> {
        if self.replicas < 1 {
            return Err(format!(
                "replicas must be at least 1, got {}",
                self.replicas
            ));
        }
        if self.max_replicas < self.replicas {
            return Err(format!(
                "maxReplicas ({}) is below replicas ({})",
                self.max_replicas, self.replicas
            ));
        }
        let (up, down) = (
            self.scaling.up_when_free_below,
            self.scaling.down_when_free_above,
        );
        if !(0.0..=1.0).contains(&up) || !(0.0..=1.0).contains(&down) {
            return Err("scaling thresholds are fractions between 0 and 1".into());
        }
        if down <= up {
            return Err(format!(
                "scaling.downWhenFreeAbove ({down}) must exceed scaling.upWhenFreeBelow ({up}); \
                 adjacent thresholds oscillate, and every cycle copies a replica's blocks"
            ));
        }
        if let Some(uri) = &self.offload {
            let Some(root) = cold_mount(uri) else {
                return Err(format!(
                    "spec.offload ({uri}) must be a file:// URL with an absolute path; \
                     it is the only scheme Mira's offload target parses"
                ));
            };
            if self.cold_storage_claim.is_none() {
                return Err(format!(
                    "spec.offload is set but spec.coldStorageClaim is not; the drain Job would \
                     write the archive to {root} inside its own container and the volume would \
                     be deleted anyway. Name a claim to mount there"
                ));
            }
        }
        Ok(())
    }
}

/// Where the drain Job has to mount the cold store for `offload` to land on it:
/// the first path segment of the URL.
///
/// `file:///cold/${node}` mounts at `/cold`, and `mira offload push` creates the
/// per-node directory underneath. Mounting the full path instead would give
/// every replica its own volume, which is one claim per drain forever.
pub fn cold_mount(offload: &str) -> Option<String> {
    let path = offload.strip_prefix("file://")?;
    let first = path.strip_prefix('/')?.split('/').next()?;
    (!first.is_empty()).then(|| format!("/{first}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MiraClusterSpec {
        MiraClusterSpec {
            image: "ghcr.io/trianalab/mira:0.0.4".into(),
            replicas: 1,
            max_replicas: 10,
            storage: Storage {
                size: Quantity("10Gi".into()),
                class_name: None,
            },
            scaling: Scaling::default(),
            offload: None,
            cold_storage_claim: None,
            proxy: Proxy::default(),
        }
    }

    /// The defaults have to leave a gap, or every cluster that sets no
    /// thresholds at all is the oscillating one.
    #[test]
    fn the_default_thresholds_do_not_oscillate() {
        assert!(spec().validate().is_ok());
        assert!(default_scale_down_above() > default_scale_up_below());
    }

    /// Thresholds that meet are the expensive failure — each action creates the
    /// condition for its opposite, and a scale-in is not free the way a
    /// Deployment's is: it moves a replica's whole dataset through `offload
    /// push` before the volume goes.
    #[test]
    fn thresholds_that_cross_or_touch_are_refused() {
        for (up, down) in [(0.5, 0.5), (0.6, 0.3), (0.15, 0.15)] {
            let mut s = spec();
            s.scaling.up_when_free_below = up;
            s.scaling.down_when_free_above = down;
            let e = s.validate().unwrap_err();
            assert!(e.contains("oscillate"), "{up}/{down} was allowed: {e}");
        }
    }

    /// A ceiling below the floor asks for a tier that is simultaneously too big
    /// and too small; caught here rather than as a StatefulSet that flaps.
    #[test]
    fn a_ceiling_below_the_floor_is_refused() {
        let mut s = spec();
        s.replicas = 5;
        s.max_replicas = 3;
        assert!(s.validate().unwrap_err().contains("maxReplicas"));

        s.replicas = 0;
        s.max_replicas = 10;
        assert!(s.validate().unwrap_err().contains("at least 1"));
    }

    /// A fraction outside 0..=1 is a unit mistake — someone writing 15 for
    /// fifteen percent. It would otherwise mean "scale out always".
    #[test]
    fn a_threshold_outside_zero_to_one_is_refused() {
        let mut s = spec();
        s.scaling.up_when_free_below = 15.0;
        assert!(s.validate().unwrap_err().contains("fractions"));
    }

    /// The one that was silent data loss before this check existed.
    ///
    /// `offload` alone is a drain Job that writes the archive into its own
    /// container, exits 0, and lets the operator delete the volume it thinks it
    /// archived. There is no error anywhere in that sequence — the phase goes
    /// `Ready` — so the only place it can be caught is before it starts.
    #[test]
    fn an_offload_with_nowhere_to_write_is_refused() {
        let mut s = spec();
        s.offload = Some("file:///cold/${node}".into());
        let e = s.validate().unwrap_err();
        assert!(e.contains("coldStorageClaim"), "{e}");

        s.cold_storage_claim = Some("mira-cold".into());
        assert!(s.validate().is_ok());
    }

    /// `file://` is the only scheme `Target::parse` accepts, so an `s3://` here
    /// is a drain that fails at the last step with the volume already gone from
    /// the StatefulSet. Refuse it while it is still a typo.
    #[test]
    fn an_offload_that_is_not_a_file_url_is_refused() {
        for uri in ["s3://bucket/cold", "/cold/${node}", "file://", "file:///"] {
            let mut s = spec();
            s.offload = Some(uri.into());
            s.cold_storage_claim = Some("mira-cold".into());
            assert!(
                s.validate().unwrap_err().contains("file://"),
                "{uri} was allowed"
            );
        }
    }

    /// The mount point is the first segment, not the whole path: `${node}`
    /// expands per replica, and mounting the expanded path would be one claim
    /// per drain forever.
    #[test]
    fn the_cold_mount_is_the_first_path_segment() {
        assert_eq!(cold_mount("file:///cold/${node}").as_deref(), Some("/cold"));
        assert_eq!(cold_mount("file:///archive").as_deref(), Some("/archive"));
        assert_eq!(cold_mount("file:///a/b/c").as_deref(), Some("/a"));
        assert_eq!(cold_mount("s3://bucket/x"), None);
        assert_eq!(cold_mount("file://relative/x"), None);
    }
}
