//! Stable entity identity — the join key that makes correlation work across
//! blocks.
//!
//! Interning gives a resource a `resource_id` that is *block-local* by design
//! (section 0). Correlation needs the opposite: a value that is identical whenever two
//! resources denote the same running thing, and different otherwise, in any
//! block, forever.
//!
//! Attribute-set equality is not that value. A pod that starts reporting one
//! extra attribute halfway through an hour would become two entities, and every
//! "show me everything from this pod" answer would silently lose half its rows.
//! That failure is invisible — the query succeeds and returns a plausible
//! subset — which makes it the worst kind of bug to ship into a correlation
//! feature.
//!
//! So identity is a hash of only the attributes OpenTelemetry semantic
//! conventions define as *identifying*, taken at the most specific level that is
//! actually present. `service.instance.id` is specified as globally unique when
//! combined with `service.name` and `service.namespace`, which makes that triple
//! the primary candidate; the rest of the ladder covers the very common case of
//! telemetry that predates it.

use mira_proto::common::v1::{AnyValue, KeyValue, any_value::Value};
use prost::Message;

/// Identity candidates, most specific first. A candidate matches when every one
/// of its required keys is present; its optional keys join the hash when they
/// are. First match wins.
///
/// This list is deliberately fixed rather than configurable: an identity rule
/// that two operators set differently is an identity rule that does not identify
/// anything. See principle 3 in docs/ARCHITECTURE.md section 1.
const IDENTITY: &[(&[&str], &[&str])] = &[
    (
        &["service.name", "service.instance.id"],
        &["service.namespace"],
    ),
    (&["k8s.pod.uid"], &["k8s.container.name"]),
    (&["container.id"], &[]),
    (&["host.id"], &["process.pid"]),
    (&["host.name"], &["process.pid"]),
    (&["service.name"], &["service.namespace"]),
];

/// FNV-1a 64 with a splitmix64 finalizer over raw bytes.
///
/// Shared with [`crate::block::node_id`], which needs the same property this
/// module was written for: the same input must hash the same way in every build,
/// forever.
pub fn hash64(bytes: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.write(bytes);
    h.finish()
}

/// No candidate matched, so this resource has no stable identity and correlation
/// by entity is not answerable for it.
///
/// The tempting fallback is to hash the whole attribute set. It is wrong for
/// exactly the reason stated at the top of this module: that hash changes when an
/// attribute is added or removed, so entity drift forks the entity anyway and
/// "everything this pod emitted" silently returns a plausible subset. A sentinel
/// makes the query layer refuse instead of guessing, and a refusal naming the
/// missing attribute is an answer the user can act on.
pub const NO_IDENTITY: u64 = 0;

/// The stable 64-bit identity of the entity described by `attrs`, or
/// [`NO_IDENTITY`] if nothing identifying is present.
pub fn resource_key(attrs: &[KeyValue]) -> u64 {
    for (n, (required, optional)) in IDENTITY.iter().enumerate() {
        if !required.iter().all(|k| find(attrs, k).is_some()) {
            continue;
        }
        // The candidate index is folded in so that `host.id = "abc"` and
        // `container.id = "abc"` cannot collide.
        let mut h = Fnv::new();
        h.write(&[n as u8]);
        for k in required.iter().chain(optional.iter()) {
            if let Some(v) = find(attrs, k) {
                h.write(k.as_bytes());
                h.write(&[0]);
                h.value(v);
                h.write(&[0]);
            }
        }
        // The sentinel must never be reachable from the hash, or an unidentified
        // resource would silently join with an identified one.
        let key = h.finish();
        return if key == NO_IDENTITY { 1 } else { key };
    }

    NO_IDENTITY
}

fn find<'a>(attrs: &'a [KeyValue], key: &str) -> Option<Option<&'a AnyValue>> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_ref())
}

/// FNV-1a 64 with a splitmix64 finalizer. Hand-rolled because the hash has to be
/// byte-stable across builds and across releases — `DefaultHasher` explicitly is
/// not, and a key that changes when the binary is upgraded is not an identity.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn value(&mut self, v: Option<&AnyValue>) {
        match v.and_then(|v| v.value.as_ref()) {
            // Strings are the whole identity set in practice; hash them without
            // the protobuf round trip.
            Some(Value::StringValue(s)) => self.write(s.as_bytes()),
            Some(other) => {
                let owned = AnyValue {
                    value: Some(other.clone()),
                };
                self.write(&owned.encode_to_vec());
            }
            None => {}
        }
    }

    fn finish(self) -> u64 {
        mix(self.0)
    }
}

/// FNV-1a alone avalanches poorly in the high bits; this is the splitmix64
/// finalizer, which is what makes the 64 bits worth 64 bits.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(v.into())),
            }),
        }
    }

    #[test]
    fn identity_survives_attribute_drift_and_reordering() {
        let base = vec![
            kv("service.name", "checkout"),
            kv("service.instance.id", "7f3a"),
        ];
        // The same instance, later, with an extra non-identifying attribute and
        // the identifying ones in the other order. This is the case that splits
        // an entity in two if identity is attribute-set equality.
        let drifted = vec![
            kv("service.instance.id", "7f3a"),
            kv("k8s.node.name", "node-4"),
            kv("service.name", "checkout"),
        ];
        assert_eq!(resource_key(&base), resource_key(&drifted));

        // Different instance of the same service is a different entity.
        let other = vec![
            kv("service.name", "checkout"),
            kv("service.instance.id", "91bd"),
        ];
        assert_ne!(resource_key(&base), resource_key(&other));

        // Same value under a different identifying key is a different entity.
        assert_ne!(
            resource_key(&[kv("host.id", "abc")]),
            resource_key(&[kv("container.id", "abc")])
        );

        // No identifying attribute at all: the sentinel, not a hash of whatever
        // happened to be there. Any hash of the full set would make these three
        // three different entities, which is the drift bug the first assertion
        // in this test exists to forbid.
        assert_eq!(resource_key(&[kv("a", "1"), kv("b", "2")]), NO_IDENTITY);
        assert_eq!(resource_key(&[kv("a", "1")]), NO_IDENTITY);
        assert_eq!(resource_key(&[]), NO_IDENTITY);
    }

    /// Nothing in OTLP says an identifying attribute has to be a string. An SDK
    /// that reports `service.instance.id` as an integer, or a proxy that
    /// forwards a `KeyValue` with the value stripped, still describes an
    /// entity, and both have to come out of here as a stable key rather than as
    /// the sentinel or a panic.
    ///
    /// The two properties worth pinning are the ones the protobuf round trip
    /// buys: an integer `7` is not the string `"7"` — they are different
    /// entities, so a join must not merge them — and a valueless attribute is
    /// still *present*, so it selects the candidate its key belongs to rather
    /// than falling through to a less specific one.
    #[test]
    fn a_non_string_identifying_value_is_still_an_identity_and_is_not_its_own_text() {
        let int = |k: &str, v: i64| KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(v)),
            }),
        };
        let numeric = vec![
            kv("service.name", "checkout"),
            int("service.instance.id", 7),
        ];

        assert_ne!(resource_key(&numeric), NO_IDENTITY);
        // Byte-stable: the same attributes hash the same way every time, which
        // is the entire contract of this module.
        assert_eq!(
            resource_key(&numeric),
            resource_key(&[
                kv("service.name", "checkout"),
                int("service.instance.id", 7)
            ])
        );
        assert_ne!(
            resource_key(&numeric),
            resource_key(&[
                kv("service.name", "checkout"),
                int("service.instance.id", 8)
            ])
        );
        assert_ne!(
            resource_key(&numeric),
            resource_key(&[
                kv("service.name", "checkout"),
                kv("service.instance.id", "7")
            ])
        );

        // Present with no value at all. `find` reports the key, so the
        // `service.name`+`service.instance.id` candidate matches and the
        // candidate index goes into the hash — which is what keeps this
        // distinct from the same resource with no instance id at all, where the
        // last candidate would have matched instead.
        let valueless = vec![
            kv("service.name", "checkout"),
            KeyValue {
                key: "service.instance.id".into(),
                value: None,
            },
        ];
        assert_ne!(resource_key(&valueless), NO_IDENTITY);
        assert_ne!(
            resource_key(&valueless),
            resource_key(&[kv("service.name", "checkout")])
        );
    }
}
