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

    /// Non-finite becomes `null`. JSON has no NaN or Infinity, and both turn up
    /// in real metrics — a histogram `sum` over no observations, a gauge from a
    /// division by zero. Emitting the bare token would produce a document that
    /// every strict parser, including the browser's, rejects outright, taking
    /// the whole response down with it rather than the one field.
    pub fn f64(&mut self, v: f64) {
        self.sep();
        if v.is_finite() {
            let mut b = ryu_lite(v);
            if b.ends_with(".0") {
                b.truncate(b.len() - 2);
            }
            self.buf.push_str(&b);
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
    // the name being honest about what it is not.
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
            r#"{"rows":[{"body":"line1\nline2\t\"quoted\"\\ \u0001","trace_id":"4bf92f"},{"n":-17}],"nan":null,"ratio":0.5,"whole":3,"empty":[]}"#
        );
    }
}
