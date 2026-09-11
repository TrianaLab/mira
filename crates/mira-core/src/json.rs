//! A JSON writer, because the alternative is 30 crates.
//!
//! Mira serializes exactly one shape — query results — and it does so from
//! hand-written code that already knows the shape statically. serde_json would
//! buy derive macros for structs that do not exist here (results are built
//! column-wise straight out of Arrow arrays, never as a tree of Rust values) at
//! the cost of serde + serde_json + syn + quote + proc-macro2 and a compile-time
//! hit on every build. This is the whole feature in a hundred lines.
//!
//! The nesting API takes a closure per container so brackets cannot be
//! unbalanced and commas cannot be missed: there is no `end_object` to forget.

/// Incrementally built JSON. `key` before a value inside `obj`, bare values
/// inside `arr`.
pub struct Json {
    buf: String,
    /// Whether the next value needs a `,` in front of it. Reset on entering a
    /// container, set after every value.
    comma: bool,
}

impl Default for Json {
    fn default() -> Self {
        Self::new()
    }
}

impl Json {
    pub fn new() -> Self {
        Self {
            buf: String::with_capacity(64 << 10),
            comma: false,
        }
    }

    pub fn into_string(self) -> String {
        self.buf
    }

    fn sep(&mut self) {
        if self.comma {
            self.buf.push(',');
        }
        self.comma = true;
    }

    /// Write a `{...}`. Inside `f`, call [`key`](Self::key) then a value.
    pub fn obj(&mut self, f: impl FnOnce(&mut Self)) {
        self.sep();
        self.buf.push('{');
        self.comma = false;
        f(self);
        self.buf.push('}');
        self.comma = true;
    }

    /// Write a `[...]`.
    pub fn arr(&mut self, f: impl FnOnce(&mut Self)) {
        self.sep();
        self.buf.push('[');
        self.comma = false;
        f(self);
        self.buf.push(']');
        self.comma = true;
    }

    /// A member name. The value that follows attaches to it, so `comma` stays
    /// as it was rather than being armed here.
    pub fn key(&mut self, k: &str) {
        self.sep();
        escape(&mut self.buf, k);
        self.buf.push(':');
        self.comma = false;
    }

    pub fn str(&mut self, s: &str) {
        self.sep();
        escape(&mut self.buf, s);
    }

    pub fn i64(&mut self, v: i64) {
        self.sep();
        self.buf.push_str(itoa(v).as_str());
    }

    pub fn u64(&mut self, v: u64) {
        self.sep();
        self.buf.push_str(&v.to_string());
    }

    /// A 64-bit integer as a JSON *string*, which is what OTLP/JSON says a
    /// 64-bit integer is.
    ///
    /// Principle 3 decides this: where OTLP specifies an encoding, OTLP wins.
    /// The spec makes `int64`, `sfixed64` and `uint64` strings on the wire, the
    /// ingest decoder already reads them that way (section 0), and a response Mira
    /// cannot feed back to itself as a request body is not a round trip. The
    /// mechanical reason is the same in both directions: a JSON number is an
    /// IEEE754 double to a browser and to most parsers, `time_unix_nano` is
    /// ~1.7e18, and 2^53 is where a double stops counting. Bare-number output
    /// does not fail there, it silently rounds.
    ///
    /// Only the 64-bit columns go through this. A `severity_number`, a
    /// `status_code` or a `dropped_*` is 32 bits or smaller, arithmetic on it
    /// is what a reader wants, and no double loses it.
    pub fn i64_str(&mut self, v: i64) {
        self.quoted_digits(&v.to_string());
    }

    pub fn u64_str(&mut self, v: u64) {
        self.quoted_digits(&v.to_string());
    }

    /// Quote without escaping: the input is `to_string` of an integer, so it is
    /// ASCII digits and at most a leading `-`.
    fn quoted_digits(&mut self, digits: &str) {
        self.sep();
        self.buf.push('"');
        self.buf.push_str(digits);
        self.buf.push('"');
    }

    /// Non-finite becomes `null`. JSON has no NaN or Infinity, and both turn up
    /// in real metrics — a histogram `sum` over no observations, a gauge from a
    /// division by zero. Emitting the bare token would produce a document that
    /// every strict parser, including the browser's, rejects outright, taking
    /// the whole response down with it rather than the one field.
    pub fn f64(&mut self, v: f64) {
        self.sep();
        if v.is_finite() {
            self.buf.push_str(&ryu_lite(v));
        } else {
            self.buf.push_str("null");
        }
    }

    pub fn bool(&mut self, v: bool) {
        self.sep();
        self.buf.push_str(if v { "true" } else { "false" });
    }

    pub fn null(&mut self) {
        self.sep();
        self.buf.push_str("null");
    }

    /// Splice in a fragment this writer produced earlier — a rendered value, or
    /// a run of `"k":v,"k":v` members inside an object.
    ///
    /// The escape hatch for the one thing the closure API cannot express:
    /// caching. A metric's descriptor and its resource attributes are identical
    /// across every point of a series, and re-rendering them per point is the
    /// difference between a chart query that costs one pass and one that costs
    /// two. Empty is a no-op so a cached fragment that turned out to be empty
    /// cannot emit a stray comma.
    pub fn raw(&mut self, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        self.sep();
        self.buf.push_str(fragment);
    }

    /// Lowercase hex of `bytes`, as one string. Trace and span ids are
    /// `FixedSizeBinary` on disk and hex everywhere a human or a W3C header
    /// sees them.
    pub fn hex(&mut self, bytes: &[u8]) {
        self.sep();
        self.buf.push('"');
        for b in bytes {
            self.buf.push(HEX[(b >> 4) as usize] as char);
            self.buf.push(HEX[(b & 0xf) as usize] as char);
        }
        self.buf.push('"');
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn itoa(v: i64) -> String {
    v.to_string()
}

fn ryu_lite(v: f64) -> String {
    // `{}` on f64 already produces the shortest representation that round-trips
    // (Rust uses Grisu/Ryū internally), so there is nothing to add here beyond
    // the name being honest about what it is not. Unlike `{:?}` it never emits
    // a trailing `.0`, so a whole number needs no trimming — see
    // `a_whole_double_needs_no_trimming_and_a_control_character_is_never_raw`.
    format!("{v}")
}

/// RFC 8259 string escaping, including the control characters below 0x20 that
/// the spec requires be escaped and that a naive writer silently emits raw.
fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                out.push(HEX[(c as usize >> 4) & 0xf] as char);
                out.push(HEX[c as usize & 0xf] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nesting, comma placement and the escapes an attacker-influenced string
    /// can carry. Any of these wrong is a response body that no strict parser
    /// will read — the whole query lost, not one field.
    #[test]
    fn nesting_commas_and_hostile_strings() {
        let mut j = Json::new();
        j.obj(|j| {
            j.key("rows");
            j.arr(|j| {
                j.obj(|j| {
                    j.key("body");
                    // A log body is the most attacker-influenced string in the
                    // system, and a raw newline or a bare 0x01 in it is what
                    // turns a query response into a parse error.
                    j.str("line1\nline2\t\"quoted\"\\ \u{1}");
                    j.key("trace_id");
                    j.hex(&[0x4b, 0xf9, 0x2f]);
                });
                j.obj(|j| {
                    j.key("n");
                    j.i64(-17);
                    // The 64-bit pair. `1e18 + 1` is the whole argument for
                    // them: as a bare number every browser reads it back as
                    // 1000000000000000000, one short, with no error anywhere.
                    j.key("big");
                    j.i64_str(-1_000_000_000_000_000_001);
                    j.key("ubig");
                    j.u64_str(u64::MAX);
                });
            });
            j.key("nan");
            j.f64(f64::NAN);
            j.key("ratio");
            j.f64(0.5);
            j.key("whole");
            j.f64(3.0);
            j.key("empty");
            j.arr(|_| {});
        });
        assert_eq!(
            j.into_string(),
            r#"{"rows":[{"body":"line1\nline2\t\"quoted\"\\ \u0001","trace_id":"4bf92f"},{"n":-17,"big":"-1000000000000000001","ubig":"18446744073709551615"}],"nan":null,"ratio":0.5,"whole":3,"empty":[]}"#
        );
    }

    /// The rest of RFC 8259's named escapes, and the one thing an empty cached
    /// fragment must never do: arm a comma. `raw("")` between two members that
    /// emitted a separator would produce `{"a":1,,"b":2}` — a body a browser
    /// rejects outright, and only on the runs where the cache happens to miss.
    #[test]
    fn an_empty_fragment_is_invisible_and_every_named_escape_is_named() {
        let mut j = Json::default();
        j.obj(|j| {
            j.key("body");
            // \b and \f have no Rust escape, and 0x1f is the last character the
            // spec requires escaped, so it is the \u00XX arm's upper boundary.
            j.str("\u{08}\u{0c}\r\n\t\u{1f} ");
            j.raw("");
            j.raw(r#""a":1"#);
            j.raw("");
            j.raw(r#""b":2"#);
        });
        assert_eq!(
            j.into_string(),
            "{\"body\":\"\\b\\f\\r\\n\\t\\u001f \",\"a\":1,\"b\":2}"
        );
    }

    /// `Display` for `f64` never writes the trailing `.0` that `Debug` does, so
    /// a whole double is already the integer a chart wants, across the whole
    /// range a metric can hold. If that ever changes, every `count` in a query
    /// response grows a `.0` and the trim this writer used to carry has to come
    /// back.
    #[test]
    fn no_finite_double_is_written_with_a_trailing_point_zero() {
        for v in [3.0f64, -0.0, 1e20, 1e-7, f64::MAX, f64::MIN_POSITIVE, 0.5] {
            let mut j = Json::new();
            j.f64(v);
            let s = j.into_string();
            assert!(!s.ends_with(".0"), "{v:?} rendered as {s}");
            assert_eq!(s.parse::<f64>().expect("round trips"), v);
        }
    }
}
