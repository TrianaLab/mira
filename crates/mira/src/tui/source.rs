//! Where the TUI's answers come from.
//!
//! Two transports, one return type: a parsed response envelope. A local source
//! calls `mira_core::query` on a block directory with no server anywhere in the
//! picture — which is the whole reason the CLI is worth having, because it means
//! a detached PVC or a dead pod's volume is still readable. A remote source
//! POSTs the identical document to `/api/v1/*` on a running replica.
//!
//! Both go through `api.rs`'s parsers and `api.rs`'s envelope, so the query
//! grammar is not reimplemented here — a filter that works against a server
//! works against a directory because it is the same code deciding what it means.
//!
//! Responses are parsed with the KYAML loader, not a JSON parser. That is the
//! KYAML-first principle paying for itself a second time: the API emits JSON,
//! JSON is valid KYAML, and `yaml-rust2` is already in the tree for the config
//! file. There is no JSON parsing dependency in this binary and this does not
//! add one.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use yaml_rust2::Yaml;

use crate::api;

pub enum Source {
    /// A block directory, read in-process.
    Local(PathBuf),
    /// `host:port` of a running Mira's HTTP listener.
    Remote(String),
}

pub const QUERY: &str = "/api/v1/query";
pub const SERIES: &str = "/api/v1/metrics/query";
pub const NAMES: &str = "/api/v1/metrics/names";

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Local(p) => format!("local {}", p.display()),
            Source::Remote(a) => format!("http {a}"),
        }
    }

    /// Run one query and hand back the parsed envelope.
    ///
    /// Errors are `String` because every one of them ends up in the status bar
    /// verbatim. A TUI that reports "query failed" and keeps the reason to
    /// itself is worse than no TUI.
    pub fn post(&self, route: &str, body: &str) -> Result<Yaml, String> {
        let text = match self {
            Source::Local(dir) => local(dir, route, body)?,
            Source::Remote(addr) => http_post(addr, route, body)?,
        };
        let doc = api::parse(&text)?;
        // The API answers errors as JSON too, so a 200 is not the only thing
        // worth checking — and on the local path there is no status code at all.
        match doc["error"].as_str() {
            Some(e) => Err(e.to_owned()),
            None => Ok(doc),
        }
    }
}

fn local(dir: &Path, route: &str, body: &str) -> Result<String, String> {
    let now = api::now_nanos();
    let run = |field, r: mira_core::error::Result<mira_core::query::Results>| match r {
        Ok(r) => Ok(api::envelope(field, &r)),
        Err(e) => Err(e.to_string()),
    };
    match route {
        QUERY => run(
            "rows",
            mira_core::query::search(dir, &api::parse_search(body, now)?),
        ),
        SERIES => run(
            "series",
            mira_core::series::series(dir, &api::parse_series(body, now)?),
        ),
        NAMES => {
            let (from, to) = api::window(body, now)?;
            run("names", mira_core::series::names(dir, from, to))
        }
        other => Err(format!("no such route {other}")),
    }
}

/// HTTP/1.1 POST, one connection per request.
///
/// ponytail: `Connection: close` and `read_to_end`, which is what lets this
/// skip chunked-transfer decoding and keep-alive bookkeeping entirely — the
/// server closes the socket and EOF delimits the body. The ceiling is one TCP
/// handshake per query, invisible next to the query itself; add pooling if the
/// TUI ever polls faster than a human types. No TLS: Mira serves plain HTTP, and
/// a TLS client is where the dependency budget would actually go.
fn http_post(addr: &str, path: &str, body: &str) -> Result<String, String> {
    let io = || -> std::io::Result<Vec<u8>> {
        let mut s = TcpStream::connect(addr)?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        write!(
            s,
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        s.flush()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        Ok(buf)
    };
    let raw = io().map_err(|e| format!("{addr}: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{addr}: response has no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let text = String::from_utf8_lossy(&raw[split + 4..]).into_owned();

    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("?");
    match status {
        "200" => Ok(text),
        // The body is the JSON error envelope; hand it up so `post` can pull the
        // message out of it rather than showing the caller a bare number.
        _ if text.trim_start().starts_with('{') => Ok(text),
        _ => Err(format!("{addr} returned HTTP {status}: {}", text.trim())),
    }
}

/// Normalise what someone types after `--addr` into `host:port`.
///
/// `http://` because that is what they will copy out of a browser, and a bare
/// host because that is what they will type. Port 4318 is where the query API
/// lives, so it is the only sensible default.
pub fn parse_addr(s: &str) -> Result<String, String> {
    let s = s
        .trim()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim();
    if s.starts_with("https://") {
        return Err("--addr: Mira serves plain HTTP; there is no TLS client here".into());
    }
    if s.is_empty() {
        return Err("--addr needs a host".into());
    }
    // Only a bare `host` needs the default appended. An IPv6 literal already
    // carries colons inside its brackets, so counting them would be wrong.
    Ok(
        match s
            .rsplit(':')
            .next()
            .is_some_and(|p| p.parse::<u16>().is_ok())
        {
            true => s.to_owned(),
            false => format!("{s}:4318"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_normalise_to_host_port() {
        assert_eq!(parse_addr("localhost").unwrap(), "localhost:4318");
        assert_eq!(parse_addr("http://mira:4318/").unwrap(), "mira:4318");
        assert_eq!(parse_addr("10.0.0.4:9000").unwrap(), "10.0.0.4:9000");
        assert_eq!(parse_addr("[::1]:4318").unwrap(), "[::1]:4318");
        // No port, and the last colon-segment is not a number: still a host.
        assert_eq!(parse_addr("[::1]").unwrap(), "[::1]:4318");
        assert!(parse_addr("https://mira").is_err());
        assert!(parse_addr("  ").is_err());
    }

    /// The response the API emits has to survive the loader the config file
    /// uses. This is the "JSON is valid KYAML" claim, checked on the exact
    /// envelope rather than taken from the spec.
    #[test]
    fn a_response_envelope_parses_with_the_kyaml_loader() {
        let text = r#"{"rows":[{"body":"line1\nline2 \"q\" \u0001","severity_number":17,
                       "ratio":0.5,"ok":true,"gone":null,
                       "attributes":{"service.name":"checkout"}}],
                       "stats":{"blocks_total":69,"blocks_scanned":1,
                       "rows_scanned":100,"rows_matched":2}}"#;
        let d = api::parse(text).unwrap();
        let row = &d["rows"][0];
        assert_eq!(row["body"].as_str().unwrap(), "line1\nline2 \"q\" \u{1}");
        assert_eq!(row["severity_number"].as_i64().unwrap(), 17);
        assert_eq!(row["ratio"].as_f64().unwrap(), 0.5);
        assert!(row["ok"].as_bool().unwrap());
        assert!(row["gone"].is_null());
        assert_eq!(
            row["attributes"]["service.name"].as_str().unwrap(),
            "checkout"
        );
        assert_eq!(d["stats"]["blocks_scanned"].as_i64().unwrap(), 1);
    }
}
